//! Turns outlive the socket that started them (SMOODEV-3705).
//!
//! Incident 2026-10-06: Big Smooth used from an iPhone through the relay. The
//! phone backgrounded, the relay reported the peer offline, the daemon dropped
//! its loopback bridge, and the server — treating the WS close as "nobody is
//! listening" — aborted the turn mid tool call. Nothing was logged, nothing was
//! persisted, and the reply the user was waiting for never existed.
//!
//! Over a real WebSocket, this proves:
//!
//!   1. a turn whose client disconnects runs to completion and persists its reply;
//!   2. a turn that FAILS after its client left persists a turn-error record, so
//!      the reload shows what happened;
//!   3. a reconnecting client cannot start a second turn on the conversation while
//!      the first still runs (`TURN_IN_PROGRESS`), can `cancel` it by `sessionId`,
//!      and can send again once it is cancelled.
//!
//! Fully offline: a `MockLlmClient` scripts the turn and a host tool parks it
//! until the test releases it.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Notify;

use smooth_operator::adapter::MessageQuery;
use smooth_operator::domain::{Direction, Message as DomainMessage};
use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{Tool, ToolSchema};

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::runner::TURN_ERROR_METADATA_KEY;
use smooth_operator_server::server::build_state;
use smooth_operator_server::state::AppState;

const GATE_TOOL: &str = "gate_probe";

fn keyless_config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "claude-haiku-4-5".into(),
        seed_kb: false,
        max_iterations: 4,
        max_tokens: 128,
        storage: StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: Vec::new(),
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

/// Flips its flag when dropped — the positive signal that a turn future was
/// abandoned mid-await.
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Parks the turn until the test calls `release.notify_one()`.
#[derive(Clone)]
struct Gate {
    started: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
    release: Arc<Notify>,
}

impl Gate {
    fn new() -> Self {
        Self {
            started: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicBool::new(false)),
            release: Arc::new(Notify::new()),
        }
    }
}

struct GateTool(Gate);

#[async_trait]
impl Tool for GateTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: GATE_TOOL.into(),
            description: "parks the turn until released".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _arguments: Value) -> anyhow::Result<String> {
        self.0.started.store(true, Ordering::SeqCst);
        let guard = DropFlag(self.0.dropped.clone());
        self.0.release.notified().await;
        std::mem::forget(guard);
        Ok("released".into())
    }
}

struct GateProvider(Gate);

#[async_trait]
impl ToolProvider for GateProvider {
    async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(GateTool(self.0.clone()))]
    }
}

fn push_gate_call(mock: &MockLlmClient) {
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: GATE_TOOL.into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: "{}".into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ]);
}

fn push_answer(mock: &MockLlmClient, text: &str) {
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: text.into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
}

async fn boot(mock: MockLlmClient, gate: &Gate) -> (AppState, String) {
    let state = build_state(keyless_config())
        .with_chat_provider(Arc::new(mock))
        .with_tools(Arc::new(GateProvider(gate.clone())));
    let url = common::boot_state(state.clone()).await;
    (state, url)
}

/// Create a session; returns `(sessionId, conversationId)`.
async fn create_session(client: &mut common::Client) -> (String, String) {
    common::send_json(
        client,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-1",
            "agentId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    let created = common::recv_json(client).await;
    assert_eq!(created["type"], "immediate_response", "got: {created}");
    (
        created["data"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string(),
        created["data"]["conversationId"]
            .as_str()
            .expect("conversationId")
            .to_string(),
    )
}

async fn send(client: &mut common::Client, session_id: &str, request_id: &str, message: &str) {
    common::send_json(
        client,
        &json!({
            "action": "send_message",
            "requestId": request_id,
            "sessionId": session_id,
            "message": message,
        }),
    )
    .await;
}

async fn wait_until(label: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {label}");
}

async fn messages(state: &AppState, conversation_id: &str) -> Vec<DomainMessage> {
    state
        .storage
        .list_messages_by_conversation(MessageQuery::new(conversation_id, 50))
        .await
        .expect("list messages")
        .messages
}

/// Poll storage until an outbound message satisfying `pred` lands.
async fn wait_for_outbound(
    state: &AppState,
    conversation_id: &str,
    pred: impl Fn(&DomainMessage) -> bool,
) -> DomainMessage {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let found = messages(state, conversation_id)
            .await
            .into_iter()
            .find(|m| m.direction == Direction::Outbound && pred(m));
        if let Some(m) = found {
            return m;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no matching outbound message persisted; log: {:?}",
            messages(state, conversation_id).await
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_turn_whose_client_disconnects_completes_and_persists_its_reply() {
    let gate = Gate::new();
    let mock = MockLlmClient::new();
    push_gate_call(&mock);
    push_answer(&mock, "Here is the answer you waited for.");
    let (state, url) = boot(mock, &gate).await;

    let mut client = common::connect(&url).await;
    let (session_id, conversation_id) = create_session(&mut client).await;
    send(&mut client, &session_id, "turn-1", "do the slow thing").await;
    wait_until("turn parked in tool", || {
        gate.started.load(Ordering::SeqCst)
    })
    .await;

    // The phone backgrounds: the socket goes away mid tool call.
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The tool finishes after the client is gone; the turn carries on.
    gate.release.notify_one();
    let reply = wait_for_outbound(&state, &conversation_id, |m| {
        m.content.text.as_deref() == Some("Here is the answer you waited for.")
    })
    .await;
    assert!(
        reply.metadata_json.is_none(),
        "a normal reply, not an error record"
    );
    assert!(
        !gate.dropped.load(Ordering::SeqCst),
        "the turn future must never have been dropped"
    );
    wait_until("conversation released after the turn", || {
        !state.running_turns.is_running(&conversation_id)
    })
    .await;
}

#[tokio::test]
async fn a_turn_that_fails_after_its_client_left_persists_an_error_record() {
    let gate = Gate::new();
    let mock = MockLlmClient::new();
    push_gate_call(&mock);
    mock.push_stream_error("upstream exploded");
    let (state, url) = boot(mock, &gate).await;

    let mut client = common::connect(&url).await;
    let (session_id, conversation_id) = create_session(&mut client).await;
    send(&mut client, &session_id, "turn-err", "do the slow thing").await;
    wait_until("turn parked in tool", || {
        gate.started.load(Ordering::SeqCst)
    })
    .await;
    drop(client);
    gate.release.notify_one();

    let record = wait_for_outbound(&state, &conversation_id, |m| {
        m.metadata_json
            .as_ref()
            .is_some_and(|meta| meta.get(TURN_ERROR_METADATA_KEY).is_some())
    })
    .await;
    let meta = &record.metadata_json.as_ref().expect("metadata")[TURN_ERROR_METADATA_KEY];
    assert_eq!(meta["code"], "AGENT_ERROR");
    assert_eq!(meta["requestId"], "turn-err");
    let text = record.content.text.as_deref().unwrap_or_default();
    assert!(
        text.contains("failed"),
        "the record says the turn failed: {text}"
    );

    // The user's own message is still there, ahead of the record.
    let log = messages(&state, &conversation_id).await;
    assert_eq!(log.first().map(|m| m.direction), Some(Direction::Inbound));
}

#[tokio::test]
async fn a_reconnecting_client_gets_turn_in_progress_and_can_cancel_by_session() {
    let gate = Gate::new();
    let mock = MockLlmClient::new();
    push_gate_call(&mock);
    push_answer(&mock, "fresh turn");
    let (state, url) = boot(mock, &gate).await;

    let mut first = common::connect(&url).await;
    let (session_id, conversation_id) = create_session(&mut first).await;
    send(&mut first, &session_id, "turn-1", "do the slow thing").await;
    wait_until("turn parked in tool", || {
        gate.started.load(Ordering::SeqCst)
    })
    .await;
    drop(first);

    // The client comes back on a NEW socket while the first turn still runs.
    let mut second = common::connect(&url).await;
    send(&mut second, &session_id, "turn-2", "are you there?").await;
    let err = common::recv_json(&mut second).await;
    assert_eq!(err["type"], "error", "got: {err}");
    assert_eq!(err["error"]["code"], "TURN_IN_PROGRESS", "got: {err}");
    assert_eq!(err["requestId"], "turn-2");

    // It stops the orphaned turn by naming the session.
    common::send_json(
        &mut second,
        &json!({ "action": "cancel", "requestId": "stop-1", "sessionId": session_id }),
    )
    .await;
    let cancelled = common::recv_json(&mut second).await;
    assert_eq!(cancelled["type"], "cancelled", "got: {cancelled}");
    assert_eq!(
        cancelled["requestId"], "turn-1",
        "the event echoes the cancelled turn's requestId"
    );
    wait_until("orphaned turn future dropped", || {
        gate.dropped.load(Ordering::SeqCst)
    })
    .await;
    assert!(!state.running_turns.is_running(&conversation_id));

    // And the conversation takes a new turn straight away.
    send(&mut second, &session_id, "turn-3", "hello again").await;
    let mut seen = Vec::new();
    let done = common::recv_until(
        &mut second,
        "eventual_response",
        &mut seen,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(done["requestId"], "turn-3", "got: {done}");
}

#[tokio::test]
async fn a_cancel_naming_a_session_with_nothing_running_is_a_noop() {
    let gate = Gate::new();
    let (_state, url) = boot(MockLlmClient::new(), &gate).await;
    let mut client = common::connect(&url).await;
    let (session_id, _) = create_session(&mut client).await;

    for sid in [session_id.as_str(), "00000000-0000-0000-0000-000000000000"] {
        common::send_json(
            &mut client,
            &json!({ "action": "cancel", "requestId": "c", "sessionId": sid }),
        )
        .await;
    }
    common::send_json(&mut client, &json!({ "action": "ping", "requestId": "p1" })).await;
    let ev = common::recv_json(&mut client).await;
    assert_eq!(ev["type"], "pong", "cancel must emit nothing; got: {ev}");
}
