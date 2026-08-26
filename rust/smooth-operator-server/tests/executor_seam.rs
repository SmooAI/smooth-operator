//! `AppState::with_executor` — the host seam on the turn's execution (pearl th-39999c).
//!
//! `TurnRequest::executor` has always been a public field, but the server's sole
//! construction site hardcoded `None`, so no host could reach it. That left chat-ws
//! — which lets the published runner own the whole turn — with **no seam on the
//! emitted reply**: the TS general agent's post-response guard (strip an escalation
//! claim the tools never backed) and the voice stall retry had nowhere to run, and
//! the only available fix was prompt prevention.
//!
//! These drive the real `handler::handle_frame` against in-memory storage and a
//! `MockLlmClient` — the same offline harness as `skill_field.rs` — to pin the two
//! halves of the contract:
//!
//!   1. an executor installed on `AppState` runs THIS turn, and what it leaves on
//!      the returned `Conversation` is what the turn persists and what the
//!      `eventual_response` carries — i.e. a decorator can guard the reply;
//!   2. no executor installed ⇒ byte-for-byte the old behavior.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{MessageQuery, StorageAdapter};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::agent::{Agent, AgentEvent};
use smooth_operator_core::conversation::Conversation;
use smooth_operator_core::executor::{AgentExecutor, InProcessExecutor};
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler;
use smooth_operator_server::state::AppState;

/// What the model says, unguarded: an escalation claim with no `notify_humans`
/// call behind it — the exact failure the TS guard existed to catch.
const PHANTOM_CLAIM: &str = "I've passed it along to a human who will follow up.";
/// What the guard leaves in its place.
const GUARDED: &str = "I can't reach a human from here, but I can keep helping.";

/// A host post-response guard, shaped like the one chat-ws needs: delegate the
/// turn to the in-process executor, then inspect the conversation the turn
/// produced — including its tool calls — and rewrite the final assistant message
/// when the reply claims an escalation no tool actually performed.
///
/// This is the whole point of the seam: `Conversation.messages` is public, so a
/// host decorator is the one place outside the crate that can see the emitted
/// text next to the tools that ran.
struct EscalationGuardExecutor {
    inner: InProcessExecutor,
    runs: AtomicUsize,
}

impl EscalationGuardExecutor {
    fn new() -> Self {
        Self {
            inner: InProcessExecutor::new(),
            runs: AtomicUsize::new(0),
        }
    }

    fn guard(&self, mut conversation: Conversation) -> Conversation {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let escalated = conversation
            .messages
            .iter()
            .any(|m| m.tool_calls.iter().any(|c| c.name == "notify_humans"));
        if escalated {
            return conversation;
        }
        if let Some(last) = conversation
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == smooth_operator_core::conversation::Role::Assistant)
        {
            if last.content.contains("passed it along") {
                last.content = GUARDED.to_string();
            }
        }
        conversation
    }
}

#[async_trait]
impl AgentExecutor for EscalationGuardExecutor {
    async fn execute(&self, agent: &Agent, user_message: String) -> anyhow::Result<Conversation> {
        Ok(self.guard(self.inner.execute(agent, user_message).await?))
    }

    async fn execute_streaming(
        &self,
        agent: &Agent,
        user_message: String,
        events: UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<Conversation> {
        Ok(self.guard(
            self.inner
                .execute_streaming(agent, user_message, events)
                .await?,
        ))
    }
}

/// Keyless: the injected mock provider serves the turn, so no gateway key is
/// needed (the handler falls back to a placeholder LLM config).
fn keyless_config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "claude-haiku-4-5".into(),
        seed_kb: false,
        max_iterations: 2,
        max_tokens: 128,
        storage: StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: Vec::new(),
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

/// A mock that streams the phantom escalation claim and stops (no tool calls).
fn phantom_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: PHANTOM_CLAIM.into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

async fn drive(state: &AppState, frame: &Value) -> UnboundedReceiver<Value> {
    let (tx, rx) = unbounded_channel::<Value>();
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        None,
        &handler::UserScope::Unscoped,
        &frame.to_string(),
        &tx,
    )
    .await;
    rx
}

/// Receive events until one of `type == want` arrives; panic on timeout.
async fn recv_until(rx: &mut UnboundedReceiver<Value>, want: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut seen: Vec<String> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            Ok(Some(ev)) => {
                let ty = ev["type"].as_str().unwrap_or_default().to_string();
                if ty == want {
                    return ev;
                }
                seen.push(ty);
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    panic!("timed out waiting for '{want}'; saw: {seen:?}");
}

async fn create_session(state: &AppState) -> String {
    let mut rx = drive(
        state,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-1",
            "agentId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    let ev = recv_until(&mut rx, "immediate_response").await;
    ev["data"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

/// Every outbound (assistant) message persisted to `conversation_id`, in order.
async fn persisted_replies(storage: &InMemoryStorageAdapter, conversation_id: &str) -> Vec<String> {
    storage
        .list_messages_by_conversation(MessageQuery::new(conversation_id, 100))
        .await
        .expect("read messages")
        .messages
        .into_iter()
        .filter(|m| matches!(m.direction, smooth_operator::domain::Direction::Outbound))
        .filter_map(|m| m.content.text)
        .collect()
}

/// Run one turn and return `(eventual_response, persisted replies)`.
async fn run_turn(state: &AppState, storage: &InMemoryStorageAdapter) -> (Value, Vec<String>) {
    let session_id = create_session(state).await;
    let mut rx = drive(
        state,
        &json!({
            "action": "send_message",
            "requestId": "turn-1",
            "sessionId": session_id,
            "message": "my order never arrived",
        }),
    )
    .await;
    let ev = recv_until(&mut rx, "eventual_response").await;
    let session = state.get_session(&session_id).expect("session registered");
    let replies = persisted_replies(storage, &session.conversation_id).await;
    (ev, replies)
}

#[tokio::test]
async fn installed_executor_runs_the_turn_and_its_edit_reaches_the_reply() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let executor = Arc::new(EscalationGuardExecutor::new());
    let state = AppState::new(storage.clone(), keyless_config())
        .with_chat_provider(Arc::new(phantom_mock()))
        .with_executor(executor.clone());

    let (ev, replies) = run_turn(&state, storage.as_ref()).await;

    assert_eq!(
        executor.runs.load(Ordering::SeqCst),
        1,
        "the installed executor must be the one that ran the turn"
    );
    let response = ev["data"]["data"]["response"].to_string();
    assert!(
        response.contains(GUARDED),
        "the guard's rewrite must reach eventual_response; got: {response}"
    );
    assert!(
        !response.contains("passed it along"),
        "the phantom escalation claim must not survive; got: {response}"
    );
    assert_eq!(
        replies,
        vec![GUARDED.to_string()],
        "the guarded text — not the model's — is what gets persisted"
    );
}

#[tokio::test]
async fn no_executor_leaves_the_reply_exactly_as_the_model_wrote_it() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), keyless_config())
        .with_chat_provider(Arc::new(phantom_mock()));

    let (ev, replies) = run_turn(&state, storage.as_ref()).await;

    let response = ev["data"]["data"]["response"].to_string();
    assert!(
        response.contains("passed it along"),
        "default (no executor) must be unchanged; got: {response}"
    );
    assert_eq!(replies, vec![PHANTOM_CLAIM.to_string()]);
}
