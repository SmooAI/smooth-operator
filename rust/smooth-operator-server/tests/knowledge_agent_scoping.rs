//! Per-turn knowledge **agent scoping** (SMOODEV-3292), driven through the real
//! `handler::handle_frame` create-session → send-message path, offline
//! (`MockLlmClient`).
//!
//! A host whose agents are each configured with a SUBSET of the org's knowledge
//! (SmooAI: `agents.knowledge_selection`) must narrow retrieval to the agent
//! answering the turn. `knowledge_for_access` historically received only
//! `user_id` / `groups` / `organization_id`, so the host could not tell which
//! agent a turn belonged to — and a public website agent restricted to a few
//! documents grounded on the entire org's knowledge.
//!
//! The session already records the agent the caller asked for; this proves the
//! handler now carries it onto the turn's [`AccessContext`], so it reaches the
//! storage adapter's knowledge seam. Written first; it failed before the handler
//! stamped `agent_id` (the seam saw `None`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{
    ConversationUpdate, MessagePage, MessageQuery, SessionUpdate, StorageAdapter,
};
use smooth_operator::domain::{Conversation, Message, Participant, Session};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{CheckpointStore, KnowledgeBase};

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler;
use smooth_operator_server::state::AppState;

const AGENT: &str = "agent-public-faq";

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

/// Delegates everything to an in-memory adapter but records the `agent_id` of
/// every `knowledge_for_access` call.
struct AgentRecordingAdapter {
    inner: Arc<InMemoryStorageAdapter>,
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

#[async_trait]
impl StorageAdapter for AgentRecordingAdapter {
    async fn create_conversation(
        &self,
        conversation: Conversation,
    ) -> anyhow::Result<Conversation> {
        self.inner.create_conversation(conversation).await
    }
    async fn get_conversation(&self, id: &str) -> anyhow::Result<Option<Conversation>> {
        self.inner.get_conversation(id).await
    }
    async fn list_conversations_by_org(
        &self,
        organization_id: &str,
    ) -> anyhow::Result<Vec<Conversation>> {
        self.inner.list_conversations_by_org(organization_id).await
    }
    async fn update_conversation(
        &self,
        id: &str,
        update: ConversationUpdate,
    ) -> anyhow::Result<Conversation> {
        self.inner.update_conversation(id, update).await
    }
    async fn add_participant(&self, participant: Participant) -> anyhow::Result<Participant> {
        self.inner.add_participant(participant).await
    }
    async fn get_participant(&self, id: &str) -> anyhow::Result<Option<Participant>> {
        self.inner.get_participant(id).await
    }
    async fn list_participants_by_conversation(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Vec<Participant>> {
        self.inner
            .list_participants_by_conversation(conversation_id)
            .await
    }
    async fn resolve_participant_by_external_id(
        &self,
        conversation_id: &str,
        external_id: &str,
    ) -> anyhow::Result<Option<Participant>> {
        self.inner
            .resolve_participant_by_external_id(conversation_id, external_id)
            .await
    }
    async fn append_message(&self, message: Message) -> anyhow::Result<Message> {
        self.inner.append_message(message).await
    }
    async fn get_message(&self, id: &str) -> anyhow::Result<Option<Message>> {
        self.inner.get_message(id).await
    }
    async fn list_messages_by_conversation(
        &self,
        query: MessageQuery,
    ) -> anyhow::Result<MessagePage> {
        self.inner.list_messages_by_conversation(query).await
    }
    async fn create_session(&self, session: Session) -> anyhow::Result<Session> {
        self.inner.create_session(session).await
    }
    async fn get_session(&self, session_id: &str) -> anyhow::Result<Option<Session>> {
        self.inner.get_session(session_id).await
    }
    async fn update_session(
        &self,
        session_id: &str,
        update: SessionUpdate,
    ) -> anyhow::Result<Session> {
        self.inner.update_session(session_id, update).await
    }
    async fn list_sessions_by_conversation(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Vec<Session>> {
        self.inner
            .list_sessions_by_conversation(conversation_id)
            .await
    }
    fn checkpoints(&self) -> Arc<dyn CheckpointStore> {
        self.inner.checkpoints()
    }
    fn knowledge(&self) -> Arc<dyn KnowledgeBase> {
        self.inner.knowledge()
    }
    fn knowledge_for_access(&self, access: &AccessContext) -> Arc<dyn KnowledgeBase> {
        // THE assertion seam: record the agent the turn scoped retrieval to. A
        // host would narrow retrieval to that agent's configured knowledge here.
        self.seen.lock().unwrap().push(access.agent_id.clone());
        self.inner.knowledge_for_access(access)
    }
}

async fn drive(
    state: &AppState,
    access: &AccessContext,
    frame: &Value,
) -> UnboundedReceiver<Value> {
    let (tx, rx) = unbounded_channel::<Value>();
    handler::handle_frame(
        state,
        access,
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

/// Create a session for `agent_id`, send one message on it, and return every
/// `agent_id` the knowledge seam observed during that turn.
async fn turn_for_agent(access: AccessContext, agent_id: &str) -> Vec<Option<String>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let storage = Arc::new(AgentRecordingAdapter {
        inner: Arc::new(InMemoryStorageAdapter::new()),
        seen: Arc::clone(&seen),
    });
    let state =
        AppState::new(storage, keyless_config()).with_chat_provider(Arc::new(MockLlmClient::new()));

    let mut rx = drive(
        &state,
        &access,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-1",
            "agentId": agent_id,
        }),
    )
    .await;
    let ev = recv_until(&mut rx, "immediate_response").await;
    let session_id = ev["data"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let mut rx = drive(
        &state,
        &access,
        &json!({
            "action": "send_message",
            "requestId": "turn-1",
            "sessionId": session_id,
            "message": "what are your hours?",
        }),
    )
    .await;
    recv_until(&mut rx, "eventual_response").await;

    let observed = seen.lock().unwrap().clone();
    assert!(
        !observed.is_empty(),
        "knowledge_for_access must be called during the turn"
    );
    observed
}

/// An anonymous widget visitor's turn carries the session's agent to the seam.
#[tokio::test]
async fn anonymous_turn_carries_the_sessions_agent_to_knowledge_for_access() {
    let observed = turn_for_agent(AccessContext::anonymous(), AGENT).await;
    assert!(
        observed.iter().all(|a| a.as_deref() == Some(AGENT)),
        "every retrieval handle for the turn must be scoped to the session's agent, saw {observed:?}"
    );
}

/// An authenticated principal (whose context already carries its org) gets the
/// session's agent stamped too — the org branch must not skip it.
#[tokio::test]
async fn authed_turn_carries_the_sessions_agent_to_knowledge_for_access() {
    let access = AccessContext::for_user("user-1").with_organization_id("org-acme");
    let observed = turn_for_agent(access, AGENT).await;
    assert!(
        observed.iter().all(|a| a.as_deref() == Some(AGENT)),
        "every retrieval handle for the turn must be scoped to the session's agent, saw {observed:?}"
    );
}
