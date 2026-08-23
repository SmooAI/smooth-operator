//! A storage blip is not an existence claim.
//!
//! `AppState::load_session` hydrates a session from storage on a local-registry
//! miss (th-ca579c), which is the normal path for a returning visitor whose
//! WebSocket lands on a pod that has never seen their session. That read used to
//! collapse `Err` into `None`, so a Postgres hiccup was indistinguishable from a
//! session that genuinely does not exist — and every caller renders `None` as
//! `session '<id>' not found`, in a live visitor's chat bubble.
//!
//! These tests drive the real `handler::handle_frame` against a storage adapter
//! whose `get_session` can be made to fail on demand, and assert BOTH halves:
//!
//!   - a failing `get_session` produces a retryable `STORAGE_ERROR`, and never a
//!     not-found code or the words "not found";
//!   - with storage healthy, an unknown session id STILL produces
//!     `SESSION_NOT_FOUND` — the fix must not turn a real not-found into a
//!     retry-forever loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
use smooth_operator_core::{CheckpointStore, KnowledgeBase};

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler::{self, UserScope};
use smooth_operator_server::state::AppState;

/// A `StorageAdapter` that delegates everything to an in-memory adapter, except
/// that `get_session` fails while [`fail`](Self::fail) is raised — the transient
/// backend blip, on the one read this fix is about.
struct FlakySessionAdapter {
    inner: Arc<InMemoryStorageAdapter>,
    fail: Arc<AtomicBool>,
}

impl FlakySessionAdapter {
    fn new() -> (Self, Arc<AtomicBool>) {
        let fail = Arc::new(AtomicBool::new(false));
        (
            Self {
                inner: Arc::new(InMemoryStorageAdapter::new()),
                fail: Arc::clone(&fail),
            },
            fail,
        )
    }
}

#[async_trait]
impl StorageAdapter for FlakySessionAdapter {
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
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("connection reset by peer");
        }
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
}

fn base_config() -> ServerConfig {
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

/// Drive one frame through the real dispatcher and return the first event.
/// Unscoped + no org: the ownership/tenant checks pass trivially, so the only
/// thing under test is the session read.
async fn drive(state: &AppState, frame: &Value) -> Value {
    let (tx, mut rx): (_, UnboundedReceiver<Value>) = unbounded_channel();
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-blip",
        None,
        None,
        &UserScope::Unscoped,
        &frame.to_string(),
        &tx,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an event should be emitted")
        .expect("sink open")
}

/// Every action that takes a client-supplied `sessionId`, with the error code it
/// emits when the session is genuinely unknown. All six route through
/// `scoped_session`, so all six must switch to `STORAGE_ERROR` on a blip.
fn session_frames() -> Vec<(Value, &'static str)> {
    vec![
        (
            json!({"action": "get_session", "requestId": "r1", "sessionId": "s-ghost"}),
            "SESSION_NOT_FOUND",
        ),
        (
            json!({"action": "get_conversation_messages", "requestId": "r2", "sessionId": "s-ghost"}),
            "SESSION_NOT_FOUND",
        ),
        (
            json!({"action": "send_message", "requestId": "r3", "sessionId": "s-ghost", "message": "hi"}),
            "SESSION_NOT_FOUND",
        ),
        (
            json!({"action": "verify_otp", "requestId": "r4", "sessionId": "s-ghost", "code": "123456"}),
            "SESSION_NOT_FOUND",
        ),
        (
            json!({"action": "confirm_tool_action", "requestId": "r5", "sessionId": "s-ghost", "approved": true}),
            "NO_PENDING_CONFIRMATION",
        ),
        (
            json!({"action": "submit_interaction", "requestId": "r6", "sessionId": "s-ghost", "interactionId": "i-1", "values": {}}),
            "NO_PENDING_INTERACTION",
        ),
    ]
}

/// THE regression: storage down must never be rendered as "your session does not
/// exist". A visitor told that clears their conversation; a visitor told to retry
/// keeps it.
#[tokio::test]
async fn a_storage_blip_is_never_reported_as_not_found() {
    let (adapter, fail) = FlakySessionAdapter::new();
    let state = AppState::new(Arc::new(adapter), base_config());
    fail.store(true, Ordering::SeqCst);

    // Every action is checked before failing, so one run reports every handler
    // that regressed — not just the first.
    let mut regressed = Vec::new();
    for (frame, notfound_code) in session_frames() {
        let ev = drive(&state, &frame).await;
        let code = ev["error"]["code"].as_str().unwrap_or_default().to_string();
        let message = ev["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if code != "STORAGE_ERROR"
            || code == notfound_code
            || message.to_lowercase().contains("not found")
        {
            regressed.push(format!("{}: {ev}", frame["action"]));
        }
    }
    assert!(
        regressed.is_empty(),
        "these actions turned a storage blip into an existence claim:\n{}",
        regressed.join("\n")
    );
}

/// The other half — and the reason this needs two tests. A genuinely unknown id
/// must still be not-found; a fix that reported everything as retryable would
/// leave a client retrying an id that will never resolve.
#[tokio::test]
async fn an_unknown_session_is_still_not_found_when_storage_is_healthy() {
    let (adapter, fail) = FlakySessionAdapter::new();
    let state = AppState::new(Arc::new(adapter), base_config());
    assert!(!fail.load(Ordering::SeqCst), "storage must be healthy here");

    let mut wrong = Vec::new();
    for (frame, notfound_code) in session_frames() {
        let ev = drive(&state, &frame).await;
        if ev["error"]["code"].as_str().unwrap_or_default() != notfound_code {
            wrong.push(format!("{} (want {notfound_code}): {ev}", frame["action"]));
        }
    }
    assert!(
        wrong.is_empty(),
        "these actions stopped reporting a genuinely unknown id as not-found:\n{}",
        wrong.join("\n")
    );
}

/// A session that IS in storage still resolves through the blip-aware path — the
/// error branch must not swallow the happy one.
#[tokio::test]
async fn a_healthy_hydrate_still_serves_the_session() {
    let (adapter, _fail) = FlakySessionAdapter::new();
    let adapter = Arc::new(adapter);
    adapter
        .create_session(Session {
            session_id: "s-real".into(),
            conversation_id: "conv-1".into(),
            organization_id: "org-1".into(),
            agent_id: None,
            agent_name: "Agent".into(),
            user_participant_id: "p-user".into(),
            agent_participant_id: "p-agent".into(),
            thread_id: "thread-1".into(),
            status: None,
            token_count: None,
            message_count: None,
            metadata: None,
            created_at: None,
            updated_at: None,
            ended_at: None,
            last_activity_at: None,
        })
        .await
        .expect("seed the session");

    // A fresh pod: storage has it, the local registry has never seen it.
    let state = AppState::new(adapter, base_config());
    let ev = drive(
        &state,
        &json!({"action": "get_session", "requestId": "r", "sessionId": "s-real"}),
    )
    .await;

    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(ev["data"]["sessionId"], "s-real");
}
