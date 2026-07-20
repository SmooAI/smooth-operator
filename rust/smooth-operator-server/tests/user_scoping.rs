//! Per-user conversation scoping (pearl th-b2c60b) — SECURITY.
//!
//! `list_conversations` used to be scoped by ORG only, and the resume /
//! `get_conversation_messages` paths weren't owner-checked at all: any
//! authenticated member of an org could enumerate and open every other member's
//! conversations. These tests drive the real `handler::handle_frame` from the
//! attacker's side — two users in the SAME org — and assert:
//!
//!   - a user's list contains only their own conversations;
//!   - resuming, or reading messages from, another user's conversation is
//!     **byte-identical** to the genuinely-not-found response (no existence
//!     oracle to enumerate ids with);
//!   - a client-supplied `userEmail` cannot assume another user's scope — the
//!     connection's authenticated principal always wins;
//!   - auth-disabled (local daemon / `LocalServer`) stays unscoped;
//!   - auth-enabled but no principal email fails CLOSED, never unscoped.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::domain::{Direction, Message, MessageContent};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler::{self, UserScope};
use smooth_operator_server::server::SEED_ORG_ID;
use smooth_operator_server::state::AppState;

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

fn scoped(email: &str) -> UserScope {
    UserScope::User(email.to_string())
}

/// Drive one frame as `scope` and return the first emitted event.
async fn drive(state: &AppState, scope: &UserScope, frame: &Value) -> Value {
    let (tx, mut rx) = unbounded_channel::<Value>();
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        Some(SEED_ORG_ID),
        scope,
        &frame.to_string(),
        &tx,
    )
    .await;
    recv(&mut rx).await
}

async fn recv(rx: &mut UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an event should be emitted")
        .expect("sink open")
}

/// One created session: its ids, after its participants have actually landed in
/// storage (create-session persists in a spawned task).
struct Created {
    session_id: String,
    conversation_id: String,
}

/// Create a session as `scope`, optionally passing a client-supplied
/// `userEmail` in the frame (the spoofing vector), and wait for it to persist.
async fn create_session(
    state: &AppState,
    storage: &InMemoryStorageAdapter,
    scope: &UserScope,
    claimed_email: Option<&str>,
) -> Created {
    let mut frame = json!({
        "action": "create_conversation_session",
        "requestId": "cs",
        "agentId": uuid::Uuid::new_v4().to_string(),
    });
    if let Some(email) = claimed_email {
        frame["userEmail"] = Value::from(email);
    }
    let ev = drive(state, scope, &frame).await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");

    let created = Created {
        session_id: ev["data"]["sessionId"].as_str().expect("sessionId").into(),
        conversation_id: ev["data"]["conversationId"]
            .as_str()
            .expect("conversationId")
            .into(),
    };
    await_persisted(state, storage, &created).await;
    created
}

/// Poll until the spawned persistence task has written the participants and
/// registered the session, so ownership checks see a settled world.
async fn await_persisted(state: &AppState, storage: &InMemoryStorageAdapter, created: &Created) {
    for _ in 0..100 {
        let participants = storage
            .list_participants_by_conversation(&created.conversation_id)
            .await
            .expect("list participants");
        if participants.len() >= 2 && state.get_session(&created.session_id).is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("create-session never persisted for {}", created.session_id);
}

/// Append one inbound message, so the conversation isn't filtered out of
/// `list_conversations` as an empty.
async fn seed_message(storage: &InMemoryStorageAdapter, conversation_id: &str, text: &str) {
    storage
        .append_message(Message {
            id: uuid::Uuid::new_v4().to_string(),
            external_id: None,
            organization_id: Some(SEED_ORG_ID.into()),
            conversation_id: Some(conversation_id.into()),
            direction: Direction::Inbound,
            content: MessageContent::from_text(text),
            from: None,
            to: None,
            metadata_json: None,
            analytics_json: None,
            created_at: chrono::Utc::now(),
            updated_at: None,
        })
        .await
        .expect("append message");
}

async fn list_ids(state: &AppState, scope: &UserScope) -> Vec<String> {
    let ev = drive(
        state,
        scope,
        &json!({ "action": "list_conversations", "requestId": "lc" }),
    )
    .await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    ev["data"]["conversations"]
        .as_array()
        .expect("conversations array")
        .iter()
        .map(|c| c["conversationId"].as_str().expect("id").to_string())
        .collect()
}

async fn get_messages(state: &AppState, scope: &UserScope, session_id: &str) -> Value {
    drive(
        state,
        scope,
        &json!({
            "action": "get_conversation_messages",
            "requestId": "gcm",
            "sessionId": session_id,
        }),
    )
    .await
}

/// Two users in the SAME org, each with one conversation.
async fn two_users() -> (AppState, Arc<InMemoryStorageAdapter>, Created, Created) {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let a = create_session(&state, &storage, &scoped("alice@example.com"), None).await;
    seed_message(&storage, &a.conversation_id, "alice's private question").await;
    let b = create_session(&state, &storage, &scoped("bob@example.com"), None).await;
    seed_message(&storage, &b.conversation_id, "bob's private question").await;

    (state, storage, a, b)
}

// ---- listing ---------------------------------------------------------------

#[tokio::test]
async fn list_returns_only_the_authenticated_users_conversations() {
    let (state, _storage, a, b) = two_users().await;

    assert_eq!(
        list_ids(&state, &scoped("alice@example.com")).await,
        vec![a.conversation_id.clone()],
        "alice must not see bob's conversation"
    );
    assert_eq!(
        list_ids(&state, &scoped("bob@example.com")).await,
        vec![b.conversation_id.clone()],
        "bob must not see alice's conversation"
    );
}

#[tokio::test]
async fn list_matches_email_case_insensitively() {
    let (state, _storage, a, _b) = two_users().await;

    assert_eq!(
        list_ids(&state, &scoped("ALICE@Example.COM")).await,
        vec![a.conversation_id],
        "an IdP that cases the local part differently is still the same user"
    );
}

#[tokio::test]
async fn auth_enabled_without_a_principal_email_lists_nothing() {
    let (state, _storage, _a, _b) = two_users().await;

    assert!(
        list_ids(&state, &UserScope::Denied).await.is_empty(),
        "a connection with no user identity must NOT fall back to the whole org"
    );
}

#[tokio::test]
async fn auth_disabled_stays_unscoped() {
    let (state, _storage, a, b) = two_users().await;

    let mut ids = list_ids(&state, &UserScope::Unscoped).await;
    ids.sort();
    let mut expected = vec![a.conversation_id, b.conversation_id];
    expected.sort();
    assert_eq!(
        ids, expected,
        "the local daemon / LocalServer flavor must keep seeing every conversation"
    );
}

// ---- get_conversation_messages ---------------------------------------------

#[tokio::test]
async fn get_messages_on_another_users_session_is_identical_to_never_existed() {
    let (state, _storage, a, _b) = two_users().await;
    let bob = scoped("bob@example.com");

    // Bob points at Alice's real session id, then at an id that never existed.
    let not_yours = get_messages(&state, &bob, &a.session_id).await;
    let ghost_id = uuid::Uuid::new_v4().to_string();
    let never_existed = get_messages(&state, &bob, &ghost_id).await;

    assert_eq!(not_yours["type"], "error", "got: {not_yours}");
    assert_eq!(not_yours["error"]["code"], "SESSION_NOT_FOUND");

    // THE existence-oracle assertion: the two payloads must be identical once
    // the echoed session id (and the wall-clock timestamp, which carries no
    // information about the id) is normalized. Any other difference — a
    // distinct code, a different message, an extra field — tells an attacker
    // which ids are real, which is all enumeration needs.
    let normalize = |ev: Value, id: &str| {
        let raw = serde_json::to_string(&ev).expect("serialize");
        let mut ev: Value = serde_json::from_str(&raw.replace(id, "<ID>")).expect("deserialize");
        ev["timestamp"] = Value::from(0);
        ev
    };
    assert_eq!(
        normalize(not_yours, &a.session_id),
        normalize(never_existed, &ghost_id),
        "not-yours must be indistinguishable from never-existed"
    );
}

#[tokio::test]
async fn get_messages_on_your_own_session_still_works() {
    let (state, _storage, a, _b) = two_users().await;

    let ev = get_messages(&state, &scoped("alice@example.com"), &a.session_id).await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(ev["data"]["conversationId"], a.conversation_id);
    assert_eq!(
        ev["data"]["messages"].as_array().expect("messages").len(),
        1
    );
}

#[tokio::test]
async fn denied_scope_cannot_read_messages() {
    let (state, _storage, a, _b) = two_users().await;

    let ev = get_messages(&state, &UserScope::Denied, &a.session_id).await;
    assert_eq!(ev["error"]["code"], "SESSION_NOT_FOUND", "got: {ev}");
}

// ---- resume ----------------------------------------------------------------

#[tokio::test]
async fn resuming_another_users_conversation_is_identical_to_an_unknown_id() {
    let (state, storage, a, _b) = two_users().await;
    let bob = scoped("bob@example.com");

    let resume_frame = |cid: &str| {
        json!({
            "action": "create_conversation_session",
            "requestId": "cs",
            "agentId": "agent-fixed",
            "conversationId": cid,
        })
    };

    let stolen = drive(&state, &bob, &resume_frame(&a.conversation_id)).await;
    let ghost = drive(
        &state,
        &bob,
        &resume_frame(&uuid::Uuid::new_v4().to_string()),
    )
    .await;

    // Not-yours behaves EXACTLY like an unknown id: a fresh conversation is
    // minted (the pre-existing unknown-id behavior), never Alice's.
    assert_eq!(stolen["type"], "immediate_response", "got: {stolen}");
    assert_ne!(
        stolen["data"]["conversationId"], a.conversation_id,
        "bob must not be bound to alice's conversation"
    );

    // Same shape, same status, same field set — only the minted uuids differ,
    // so nothing distinguishes "real but not yours" from "never existed".
    assert_eq!(stolen["status"], ghost["status"]);
    let keys = |ev: &Value| {
        let mut k: Vec<String> = ev["data"]
            .as_object()
            .expect("data object")
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };
    assert_eq!(keys(&stolen), keys(&ghost));

    // And Alice's conversation is untouched: still exactly her two participants.
    let participants = storage
        .list_participants_by_conversation(&a.conversation_id)
        .await
        .expect("participants");
    assert_eq!(participants.len(), 2, "no participant grafted onto alice's");
}

#[tokio::test]
async fn resuming_your_own_conversation_still_binds_to_it() {
    let (state, storage, a, _b) = two_users().await;

    let ev = drive(
        &state,
        &scoped("alice@example.com"),
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs",
            "agentId": "agent-fixed",
            "conversationId": a.conversation_id,
        }),
    )
    .await;
    assert_eq!(
        ev["data"]["conversationId"], a.conversation_id,
        "own-conversation resume must still work: {ev}"
    );

    // The resumed session reads back the existing history.
    let created = Created {
        session_id: ev["data"]["sessionId"].as_str().expect("sessionId").into(),
        conversation_id: a.conversation_id.clone(),
    };
    await_persisted(&state, &storage, &created).await;
    let messages = get_messages(&state, &scoped("alice@example.com"), &created.session_id).await;
    assert_eq!(
        messages["data"]["messages"]
            .as_array()
            .expect("messages")
            .len(),
        1
    );
}

// ---- spoofing --------------------------------------------------------------

#[tokio::test]
async fn client_supplied_email_cannot_assume_another_users_scope() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    // Alice has a conversation.
    let alice = create_session(&state, &storage, &scoped("alice@example.com"), None).await;
    seed_message(&storage, &alice.conversation_id, "alice's private question").await;

    // Bob creates one while CLAIMING to be Alice in the frame.
    let bobs = create_session(
        &state,
        &storage,
        &scoped("bob@example.com"),
        Some("alice@example.com"),
    )
    .await;
    seed_message(&storage, &bobs.conversation_id, "bob impersonating alice").await;

    // The participant carries the PRINCIPAL's email, not the claimed one.
    let participants = storage
        .list_participants_by_conversation(&bobs.conversation_id)
        .await
        .expect("participants");
    let user = participants
        .iter()
        .find(|p| p.participant_type == smooth_operator::domain::ParticipantType::User)
        .expect("user participant");
    assert_eq!(
        user.email.as_deref(),
        Some("bob@example.com"),
        "the frame's userEmail must never override the authenticated principal"
    );

    // So the spoofed conversation lands in Bob's list, not Alice's — and Alice
    // still sees only her own.
    assert_eq!(
        list_ids(&state, &scoped("alice@example.com")).await,
        vec![alice.conversation_id],
    );
    assert_eq!(
        list_ids(&state, &scoped("bob@example.com")).await,
        vec![bobs.conversation_id],
    );
}

#[tokio::test]
async fn anonymous_create_still_honors_the_frame_email() {
    // The unauthenticated widget flow (no principal, single-user/unscoped
    // deployment) has no principal email to stamp, so the frame's value is
    // still used — creating a session must keep working without auth.
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let created = create_session(
        &state,
        &storage,
        &UserScope::Unscoped,
        Some("visitor@example.com"),
    )
    .await;

    let participants = storage
        .list_participants_by_conversation(&created.conversation_id)
        .await
        .expect("participants");
    let user = participants
        .iter()
        .find(|p| p.participant_type == smooth_operator::domain::ParticipantType::User)
        .expect("user participant");
    assert_eq!(user.email.as_deref(), Some("visitor@example.com"));
}
