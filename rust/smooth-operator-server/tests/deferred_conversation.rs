//! A conversation is born on its first message, not on the widget open
//! (SMOODEV-3057).
//!
//! Opening the widget used to write a `conversations` row — plus both
//! participants and a session — before the visitor had typed anything. In a
//! 30-day production sample **44 of 117** web conversations carried zero
//! messages: bare opens occupying an inbox row. And because a web create feeds a
//! fresh UUID as the conversation's `idempotency_key`, the unique index's
//! `ON CONFLICT DO NOTHING` could never collapse a double-connect the way it does
//! for sms/slack/discord, so a reconnecting visitor accumulated rows.
//!
//! These tests pin the whole contract, including the two ways it could go wrong:
//!
//!   - the write **order** must survive (conversation → user participant → agent
//!     participant → session). Every child row FKs the conversation, and a host
//!     adapter may hook the `user` participant write to capture the visitor into
//!     its CRM, reading phone/consent off the conversation's `metadata_json`
//!     (SmooAI's `chat-storage::crm_capture` does exactly that);
//!   - an open that already **carries identity** is a captured lead, not a bare
//!     open, so it must still write immediately — otherwise a pre-chat form
//!     submit from someone who then closes the tab silently stops reaching the
//!     CRM.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{
    ConversationUpdate, MessagePage, MessageQuery, SessionUpdate, StorageAdapter,
};
use smooth_operator::domain::{Conversation, Message, Participant, ParticipantType, Session};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{CheckpointStore, KnowledgeBase};

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler::{self, UserScope};
use smooth_operator_server::state::AppState;

const AGENT_ID: &str = "11111111-1111-1111-1111-111111111111";

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

/// A state whose turns can actually run: a resolvable gateway key + a mock LLM,
/// so `send_message` reaches the runner instead of bailing at the key gate.
fn state_with_live_turns(storage: Arc<dyn StorageAdapter>) -> AppState {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: "ok".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    AppState::new(
        storage,
        ServerConfig {
            gateway_key: Some("test-key".into()),
            ..base_config()
        },
    )
    .with_chat_provider(Arc::new(mock))
}

/// Drive one frame through the real dispatcher on connection `conn_id` and
/// return the first event it emits.
async fn drive(state: &AppState, conn_id: &str, frame: &Value) -> Value {
    let (tx, mut rx): (_, UnboundedReceiver<Value>) = unbounded_channel();
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        conn_id,
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

/// Open a session and return `(sessionId, conversationId)`.
async fn open(state: &AppState, conn_id: &str, frame: &Value) -> (String, String) {
    let ev = drive(state, conn_id, frame).await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(ev["status"], 200, "got: {ev}");
    (
        ev["data"]["sessionId"].as_str().expect("sessionId").into(),
        ev["data"]["conversationId"]
            .as_str()
            .expect("conversationId")
            .into(),
    )
}

/// The bare widget open: an agent, a display name, and no identity at all.
fn bare_open(request_id: &str) -> Value {
    json!({
        "action": "create_conversation_session",
        "requestId": request_id,
        "agentId": AGENT_ID,
        "userName": "Visitor",
    })
}

/// Poll until `conversation_id` has a persisted conversation, or give up. The
/// eager path persists in a spawned task, so a plain read can race it.
async fn await_conversation(
    storage: &InMemoryStorageAdapter,
    conversation_id: &str,
) -> Conversation {
    for _ in 0..100 {
        if let Some(conv) = storage
            .get_conversation(conversation_id)
            .await
            .expect("get conversation")
        {
            return conv;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("conversation '{conversation_id}' was never persisted");
}

// ---------------------------------------------------------------------------
// The deferral itself
// ---------------------------------------------------------------------------

/// The bug, stated as a property: opening the widget and typing nothing must
/// leave NOTHING behind — no conversation, no participants, no session — while
/// the client still gets its ids back and can send a message.
#[tokio::test]
async fn a_bare_open_writes_nothing() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let (session_id, conversation_id) = open(&state, "conn-1", &bare_open("cs-1")).await;

    assert!(
        state.is_session_deferred(&session_id),
        "the create is parked, not written"
    );
    assert!(
        storage
            .get_conversation(&conversation_id)
            .await
            .expect("get conversation")
            .is_none(),
        "a bare open must not mint an inbox row"
    );
    assert!(
        storage
            .list_participants_by_conversation(&conversation_id)
            .await
            .expect("list participants")
            .is_empty(),
        "no participants either — they FK the conversation"
    );
    assert!(
        storage
            .get_session(&session_id)
            .await
            .expect("get session")
            .is_none(),
        "no session row either"
    );
    // …but the session IS live on this pod, which is what the client's next
    // frame resolves through (a WebSocket stays pinned to one pod for its life).
    assert!(
        state.get_session(&session_id).is_some(),
        "the session must still be usable on this connection"
    );
}

/// Two bare opens — the double-connect that produced two inbox rows for one
/// visitor. Neither speaks, so neither earns a row.
#[tokio::test]
async fn two_bare_opens_leave_no_duplicate_rows() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    open(&state, "conn-1", &bare_open("cs-1")).await;
    open(&state, "conn-2", &bare_open("cs-2")).await;

    assert!(
        storage
            .list_conversations_by_org(smooth_operator_server::server::SEED_ORG_ID)
            .await
            .expect("list conversations")
            .is_empty(),
        "two opens that never spoke must produce zero conversations, not two"
    );
}

/// The first message is what earns the rows — conversation, both participants,
/// the session, and the inbound message itself.
#[tokio::test]
async fn the_first_message_lands_the_create() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = state_with_live_turns(storage.clone());

    let (session_id, conversation_id) = open(&state, "conn-1", &bare_open("cs-1")).await;
    drive(
        &state,
        "conn-1",
        &json!({
            "action": "send_message",
            "requestId": "m-1",
            "sessionId": session_id,
            "message": "hello",
        }),
    )
    .await;

    let conv = await_conversation(&storage, &conversation_id).await;
    assert_eq!(conv.id, conversation_id);
    assert!(
        !state.is_session_deferred(&session_id),
        "nothing is parked once it has landed"
    );

    let participants = storage
        .list_participants_by_conversation(&conversation_id)
        .await
        .expect("list participants");
    assert_eq!(
        participants.len(),
        2,
        "user + agent participants: {participants:?}"
    );
    assert!(storage
        .get_session(&session_id)
        .await
        .expect("get session")
        .is_some());

    // And the message the deferral exists to wait for actually landed against it.
    // The turn is spawned, so poll rather than read once.
    let mut landed = false;
    for _ in 0..100 {
        let page = storage
            .list_messages_by_conversation(MessageQuery::new(&conversation_id, 10))
            .await
            .expect("list messages");
        landed = page
            .messages
            .iter()
            .any(|m| m.content.text.as_deref() == Some("hello"));
        if landed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        landed,
        "the inbound message must be persisted against the now-real conversation"
    );
}

/// The order the flush writes in is load-bearing: every child row FKs the
/// conversation, and a host adapter hooks the `user` participant write to
/// capture the visitor into its CRM — reading phone + consent off the
/// conversation's `metadata_json`, which therefore has to be there already.
#[tokio::test]
async fn the_flush_preserves_the_write_order_crm_capture_depends_on() {
    let (recording, ops, _fail) = RecordingAdapter::new();
    let state = state_with_live_turns(Arc::new(recording));

    let (session_id, _conversation_id) = open(&state, "conn-1", &bare_open("cs-1")).await;
    drive(
        &state,
        "conn-1",
        &json!({
            "action": "send_message",
            "requestId": "m-1",
            "sessionId": session_id,
            "message": "hello",
        }),
    )
    .await;
    // The flush is awaited inline by `send_message`, but the turn that follows
    // is spawned; poll until the writes settle.
    for _ in 0..100 {
        if ops.lock().expect("ops").len() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let ops = ops.lock().expect("ops").clone();
    assert_eq!(
        &ops[..4],
        &[
            "create_conversation",
            "add_participant:user",
            "add_participant:ai_agent",
            "create_session",
        ],
        "the deferred flush must replay the eager path's order exactly: {ops:?}"
    );
}

// ---------------------------------------------------------------------------
// What must NOT be deferred
// ---------------------------------------------------------------------------

/// A pre-chat form submit is a captured lead, not a bare open. The `user`
/// participant write is what a host adapter hooks for CRM capture, so an open
/// carrying an email must still write immediately — otherwise a visitor who
/// fills the form and closes the tab silently stops reaching the CRM.
#[tokio::test]
async fn an_open_carrying_an_email_writes_immediately() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let (session_id, conversation_id) = open(
        &state,
        "conn-1",
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-1",
            "agentId": AGENT_ID,
            "userName": "Ada Lovelace",
            "userEmail": "ada@example.com",
        }),
    )
    .await;

    await_conversation(&storage, &conversation_id).await;
    assert!(
        !state.is_session_deferred(&session_id),
        "an identified open is never parked"
    );
    let participants = storage
        .list_participants_by_conversation(&conversation_id)
        .await
        .expect("list participants");
    assert!(
        participants
            .iter()
            .any(|p| p.participant_type == ParticipantType::User
                && p.email.as_deref() == Some("ada@example.com")),
        "the user participant carrying the captured email must be written: {participants:?}"
    );
}

/// ADR-048 puts the pre-chat form's phone at `metadata.userPhone` (the
/// participant carries no phone), so that field counts as identity too.
#[tokio::test]
async fn an_open_carrying_metadata_user_phone_writes_immediately() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let (session_id, conversation_id) = open(
        &state,
        "conn-1",
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-1",
            "agentId": AGENT_ID,
            "userName": "Ada Lovelace",
            "metadata": { "userPhone": "+15555550123" },
        }),
    )
    .await;

    await_conversation(&storage, &conversation_id).await;
    assert!(!state.is_session_deferred(&session_id));
}

/// A blank phone is not identity — it is an empty form field, and treating it as
/// one would re-mint an inbox row for every bare open the widget pads.
#[tokio::test]
async fn a_blank_metadata_user_phone_is_not_identity() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let (session_id, _conversation_id) = open(
        &state,
        "conn-1",
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-1",
            "agentId": AGENT_ID,
            "userName": "Visitor",
            "metadata": { "userPhone": "   " },
        }),
    )
    .await;

    assert!(state.is_session_deferred(&session_id));
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// The visitor closed the tab without typing: the parked create is dropped, so
/// it never becomes an inbox row AND never accumulates in the pending map for
/// the pod's lifetime.
#[tokio::test]
async fn a_closed_connection_discards_its_parked_create() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let (session_a, conversation_a) = open(&state, "conn-1", &bare_open("cs-1")).await;
    let (session_b, _conversation_b) = open(&state, "conn-2", &bare_open("cs-2")).await;

    state.discard_pending_sessions_for_conn("conn-1");

    assert!(
        !state.is_session_deferred(&session_a),
        "the closed connection's parked create is gone"
    );
    assert!(
        state.is_session_deferred(&session_b),
        "another connection's parked create is untouched"
    );
    // Dropped, not flushed: it must not have become a row on the way out.
    assert!(storage
        .get_conversation(&conversation_a)
        .await
        .expect("get conversation")
        .is_none());
    assert!(state.materialize_session(&session_a).await.is_ok());
    assert!(
        storage
            .get_conversation(&conversation_a)
            .await
            .expect("get conversation")
            .is_none(),
        "materializing a discarded session is a no-op, not a resurrection"
    );
}

/// A socket blip before the first message: the reconnect names the parked
/// conversation, so it must bind to the SAME conversation rather than mint a
/// second one (which is how a reconnect used to produce a duplicate row).
#[tokio::test]
async fn a_reconnect_before_the_first_message_resumes_the_same_conversation() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let (_session_id, conversation_id) = open(&state, "conn-1", &bare_open("cs-1")).await;

    let (_resumed_session, resumed_conversation) = open(
        &state,
        "conn-2",
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-2",
            "agentId": AGENT_ID,
            "conversationId": conversation_id,
        }),
    )
    .await;

    assert_eq!(
        resumed_conversation, conversation_id,
        "the reconnect must resume the parked conversation, not mint a fresh one"
    );
    await_conversation(&storage, &conversation_id).await;
    assert_eq!(
        storage
            .list_conversations_by_org(smooth_operator_server::server::SEED_ORG_ID)
            .await
            .expect("list conversations")
            .len(),
        1,
        "one visitor, one conversation"
    );
}

/// A storage blip during the flush is retryable, not terminal: the parked writes
/// are kept, the visitor's resend finishes them, and the steps that already
/// landed are not attempted twice (a re-INSERT would hit a taken primary key).
#[tokio::test]
async fn a_failed_flush_is_retried_without_duplicating_what_landed() {
    let (adapter, _ops, fail) = RecordingAdapter::new();
    let inner = adapter.inner.clone();
    let state = state_with_live_turns(Arc::new(adapter));

    let (session_id, conversation_id) = open(&state, "conn-1", &bare_open("cs-1")).await;

    // First message: the conversation lands, the user participant fails.
    fail.store(true, Ordering::SeqCst);
    let ev = drive(
        &state,
        "conn-1",
        &json!({
            "action": "send_message",
            "requestId": "m-1",
            "sessionId": session_id,
            "message": "hello",
        }),
    )
    .await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(
        ev["data"]["error"]["code"], "STORAGE_ERROR",
        "a flush blip is retryable, not a lost session: {ev}"
    );
    assert!(
        state.is_session_deferred(&session_id),
        "the unfinished create is kept so the resend can finish it"
    );

    // Resend against healthy storage: the flush resumes at the participant it
    // stopped on and completes.
    fail.store(false, Ordering::SeqCst);
    drive(
        &state,
        "conn-1",
        &json!({
            "action": "send_message",
            "requestId": "m-2",
            "sessionId": session_id,
            "message": "hello again",
        }),
    )
    .await;

    assert!(!state.is_session_deferred(&session_id));
    assert_eq!(
        inner
            .list_conversations_by_org(smooth_operator_server::server::SEED_ORG_ID)
            .await
            .expect("list conversations")
            .len(),
        1,
        "the retry must not create a second conversation"
    );
    assert_eq!(
        inner
            .list_participants_by_conversation(&conversation_id)
            .await
            .expect("list participants")
            .len(),
        2,
        "two participants, written once each"
    );
}

// ---------------------------------------------------------------------------
// Test adapter
// ---------------------------------------------------------------------------

/// A delegating [`StorageAdapter`] that records the ORDER of the create-session
/// writes, and can fail the `user` participant write on demand (the blip in the
/// middle of a flush). Everything else passes straight through to the in-memory
/// adapter.
struct RecordingAdapter {
    inner: Arc<InMemoryStorageAdapter>,
    ops: Arc<Mutex<Vec<String>>>,
    fail_user_participant: Arc<AtomicBool>,
}

impl RecordingAdapter {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>, Arc<AtomicBool>) {
        let ops = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicBool::new(false));
        (
            Self {
                inner: Arc::new(InMemoryStorageAdapter::new()),
                ops: Arc::clone(&ops),
                fail_user_participant: Arc::clone(&fail),
            },
            ops,
            fail,
        )
    }

    fn record(&self, op: &str) {
        self.ops.lock().expect("ops").push(op.to_string());
    }
}

#[async_trait]
impl StorageAdapter for RecordingAdapter {
    async fn create_conversation(
        &self,
        conversation: Conversation,
    ) -> anyhow::Result<Conversation> {
        self.record("create_conversation");
        self.inner.create_conversation(conversation).await
    }
    async fn add_participant(&self, participant: Participant) -> anyhow::Result<Participant> {
        let is_user = participant.participant_type == ParticipantType::User;
        if is_user && self.fail_user_participant.load(Ordering::SeqCst) {
            anyhow::bail!("connection reset by peer");
        }
        self.record(if is_user {
            "add_participant:user"
        } else {
            "add_participant:ai_agent"
        });
        self.inner.add_participant(participant).await
    }
    async fn create_session(&self, session: Session) -> anyhow::Result<Session> {
        self.record("create_session");
        self.inner.create_session(session).await
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
}
