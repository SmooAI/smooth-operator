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

// ---- the WRITE path (pearl th-1b7ed0, SECURITY) ----------------------------
//
// The read-scoping above left every sessionId-taking WRITE handler loading a
// session by raw id. `send_message` was the worst: user A sending into user B's
// session replays B's history as the turn's context and streams the reply back
// to A — reading B's conversation by asking questions against it. Each handler
// now goes through the `scoped_session` chokepoint, so these drive the attack
// from A's side and assert the denial is indistinguishable from an unknown id.

/// Drive a frame and collect EVERY event it emits (a spawned turn would emit an
/// ack + stream events; a denied one emits exactly one error).
async fn drive_all(state: &AppState, scope: &UserScope, frame: &Value) -> Vec<Value> {
    let (tx, mut rx) = unbounded_channel::<Value>();
    let handle = handler::handle_frame(
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
    if let Some(handle) = handle {
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }
    drop(tx);
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    events
}

fn send_frame(session_id: &str) -> Value {
    json!({
        "action": "send_message",
        "requestId": "sm",
        "sessionId": session_id,
        "message": "what did we discuss earlier?",
    })
}

/// Strip the echoed session id and the wall-clock timestamp, so two events can
/// be compared for "is there ANY information here about whether the id is real".
fn normalize(ev: &Value, id: &str) -> Value {
    let raw = serde_json::to_string(ev).expect("serialize");
    let mut ev: Value = serde_json::from_str(&raw.replace(id, "<ID>")).expect("deserialize");
    ev["timestamp"] = Value::from(0);
    ev
}

#[tokio::test]
async fn send_message_into_another_users_session_is_refused_and_runs_no_turn() {
    let (state, storage, a, _b) = two_users().await;
    let bob = scoped("bob@example.com");

    let events = drive_all(&state, &bob, &send_frame(&a.session_id)).await;

    // Exactly one event: the not-found error. No 202 ack, so no turn was ever
    // spawned — Bob never sees a token of Alice's replayed history.
    assert_eq!(
        events.len(),
        1,
        "a denied send must emit only the error: {events:?}"
    );
    assert_eq!(events[0]["type"], "error");
    assert_eq!(events[0]["error"]["code"], "SESSION_NOT_FOUND");

    // And Alice's conversation is untouched — the attacker's message was never
    // appended to her log.
    let messages = storage
        .list_messages_by_conversation(smooth_operator::adapter::MessageQuery::new(
            &a.conversation_id,
            50,
        ))
        .await
        .expect("list messages")
        .messages;
    assert_eq!(
        messages.len(),
        1,
        "only alice's own seeded message: {messages:?}"
    );
    assert!(
        !serde_json::to_string(&messages)
            .unwrap()
            .contains("what did we discuss earlier?"),
        "the attacker's message must not land in alice's conversation"
    );
}

#[tokio::test]
async fn a_refused_send_is_identical_to_a_session_that_never_existed() {
    let (state, _storage, a, _b) = two_users().await;
    let bob = scoped("bob@example.com");

    let ghost_id = uuid::Uuid::new_v4().to_string();
    let not_yours = drive_all(&state, &bob, &send_frame(&a.session_id)).await;
    let never_existed = drive_all(&state, &bob, &send_frame(&ghost_id)).await;

    assert_eq!(
        normalize(&not_yours[0], &a.session_id),
        normalize(&never_existed[0], &ghost_id),
        "not-yours must be indistinguishable from never-existed, or the write \
         path is an existence oracle for enumerating session ids"
    );
}

#[tokio::test]
async fn send_message_on_your_own_session_still_reaches_the_turn() {
    // No gateway key is configured in these tests, so an ALLOWED send gets as
    // far as the LLM gate (`LLM_UNAVAILABLE`) — proving the scope check passed
    // without needing a live model.
    let (state, _storage, a, _b) = two_users().await;

    let events = drive_all(
        &state,
        &scoped("alice@example.com"),
        &send_frame(&a.session_id),
    )
    .await;
    assert_eq!(
        events[0]["error"]["code"], "LLM_UNAVAILABLE",
        "got: {events:?}"
    );
}

#[tokio::test]
async fn auth_disabled_send_message_is_unaffected() {
    // The `th` daemon / LocalServer embedding runs unauthenticated single-user;
    // it must still reach the turn for any session.
    let (state, _storage, a, b) = two_users().await;

    for created in [&a, &b] {
        let events = drive_all(
            &state,
            &UserScope::Unscoped,
            &send_frame(&created.session_id),
        )
        .await;
        assert_eq!(
            events[0]["error"]["code"], "LLM_UNAVAILABLE",
            "got: {events:?}"
        );
    }
}

#[tokio::test]
async fn denied_scope_cannot_send_a_message() {
    let (state, _storage, a, _b) = two_users().await;

    let events = drive_all(&state, &UserScope::Denied, &send_frame(&a.session_id)).await;
    assert_eq!(
        events[0]["error"]["code"], "SESSION_NOT_FOUND",
        "got: {events:?}"
    );
}

#[tokio::test]
async fn get_session_on_another_users_session_is_identical_to_never_existed() {
    let (state, _storage, a, _b) = two_users().await;
    let bob = scoped("bob@example.com");

    let frame = |sid: &str| json!({ "action": "get_session", "requestId": "gs", "sessionId": sid });
    let ghost_id = uuid::Uuid::new_v4().to_string();
    let not_yours = drive(&state, &bob, &frame(&a.session_id)).await;
    let never_existed = drive(&state, &bob, &frame(&ghost_id)).await;

    assert_eq!(
        not_yours["error"]["code"], "SESSION_NOT_FOUND",
        "got: {not_yours}"
    );
    assert_eq!(
        normalize(&not_yours, &a.session_id),
        normalize(&never_existed, &ghost_id),
    );

    // Alice still reads her own snapshot.
    let own = drive(&state, &scoped("alice@example.com"), &frame(&a.session_id)).await;
    assert_eq!(own["type"], "immediate_response", "got: {own}");
    assert_eq!(own["data"]["conversationId"], a.conversation_id);
}

#[tokio::test]
async fn verify_otp_on_another_users_session_is_refused() {
    let (state, _storage, a, _b) = two_users().await;

    let frame = |sid: &str| json!({ "action": "verify_otp", "requestId": "otp", "sessionId": sid, "code": "123456" });
    let ghost_id = uuid::Uuid::new_v4().to_string();
    let not_yours = drive(&state, &scoped("bob@example.com"), &frame(&a.session_id)).await;
    let never_existed = drive(&state, &scoped("bob@example.com"), &frame(&ghost_id)).await;

    assert_eq!(
        not_yours["error"]["code"], "SESSION_NOT_FOUND",
        "got: {not_yours}"
    );
    assert_eq!(
        normalize(&not_yours, &a.session_id),
        normalize(&never_existed, &ghost_id),
        "otp code submission must not reveal which session ids are real"
    );
}

#[tokio::test]
async fn confirm_tool_action_cannot_approve_another_users_parked_write() {
    let (state, _storage, a, _b) = two_users().await;

    // Park a write confirmation on ALICE's session, as the runner's bridge would.
    let (responder, mut verdicts) = unbounded_channel::<smooth_operator_core::HumanResponse>();
    state.register_confirmation(a.session_id.clone(), responder);

    let frame = |sid: &str| {
        json!({
            "action": "confirm_tool_action",
            "requestId": "cta",
            "sessionId": sid,
            "approved": true,
        })
    };

    // Bob's approval is refused with the same event an id with nothing parked
    // gets — and, critically, does NOT consume Alice's park.
    let ghost_id = uuid::Uuid::new_v4().to_string();
    let not_yours = drive(&state, &scoped("bob@example.com"), &frame(&a.session_id)).await;
    let never_existed = drive(&state, &scoped("bob@example.com"), &frame(&ghost_id)).await;
    assert_eq!(
        not_yours["error"]["code"], "NO_PENDING_CONFIRMATION",
        "got: {not_yours}"
    );
    assert_eq!(
        normalize(&not_yours, &a.session_id),
        normalize(&never_existed, &ghost_id),
    );
    assert!(
        verdicts.try_recv().is_err(),
        "bob's approval must never reach alice's parked tool call"
    );

    // Alice's own confirm still resolves it.
    let own = drive(&state, &scoped("alice@example.com"), &frame(&a.session_id)).await;
    assert_eq!(own["type"], "immediate_response", "got: {own}");
    assert!(matches!(
        verdicts.try_recv(),
        Ok(smooth_operator_core::HumanResponse::Approved)
    ));
}

#[tokio::test]
async fn submit_interaction_cannot_resolve_another_users_parked_card() {
    let (state, _storage, a, _b) = two_users().await;

    // Park a Rich Interaction on ALICE's session.
    let (responder, mut outcomes) =
        unbounded_channel::<smooth_operator::interaction::InteractionOutcome>();
    state.register_interaction(
        a.session_id.clone(),
        smooth_operator_server::state::PendingInteraction {
            interaction_id: "int-1".into(),
            kind: "identity_intake".into(),
            spec: json!({}),
            responder,
        },
    );

    let frame = |sid: &str| {
        json!({
            "action": "submit_interaction",
            "requestId": "si",
            "sessionId": sid,
            "interactionId": "int-1",
            "declined": true,
        })
    };

    let ghost_id = uuid::Uuid::new_v4().to_string();
    let not_yours = drive(&state, &scoped("bob@example.com"), &frame(&a.session_id)).await;
    let never_existed = drive(&state, &scoped("bob@example.com"), &frame(&ghost_id)).await;
    assert_eq!(
        not_yours["error"]["code"], "NO_PENDING_INTERACTION",
        "got: {not_yours}"
    );
    assert_eq!(
        normalize(&not_yours, &a.session_id),
        normalize(&never_existed, &ghost_id),
    );
    assert!(
        outcomes.try_recv().is_err(),
        "bob's submit must never resolve alice's parked interaction"
    );

    // Alice's own submit still resolves it.
    let own = drive(&state, &scoped("alice@example.com"), &frame(&a.session_id)).await;
    assert_eq!(own["type"], "immediate_response", "got: {own}");
    assert!(outcomes.try_recv().is_ok());
}

#[tokio::test]
async fn rename_cannot_retitle_another_users_conversation() {
    let (state, storage, a, _b) = two_users().await;

    let frame = |cid: &str| {
        json!({
            "action": "rename_conversation",
            "requestId": "rc",
            "conversationId": cid,
            "title": "pwned",
        })
    };

    let ghost_id = uuid::Uuid::new_v4().to_string();
    let not_yours = drive(
        &state,
        &scoped("bob@example.com"),
        &frame(&a.conversation_id),
    )
    .await;
    let never_existed = drive(&state, &scoped("bob@example.com"), &frame(&ghost_id)).await;
    assert_eq!(
        not_yours["error"]["code"], "CONVERSATION_NOT_FOUND",
        "got: {not_yours}"
    );
    assert_eq!(
        normalize(&not_yours, &a.conversation_id),
        normalize(&never_existed, &ghost_id),
    );

    let conversation = storage
        .get_conversation(&a.conversation_id)
        .await
        .expect("get conversation")
        .expect("conversation exists");
    assert_ne!(conversation.name, "pwned");

    // Alice can still rename her own.
    let own = drive(
        &state,
        &scoped("alice@example.com"),
        &frame(&a.conversation_id),
    )
    .await;
    assert_eq!(own["type"], "immediate_response", "got: {own}");
}

// ---- ownerless sessions (pearl th-909995, SECURITY) ------------------------
//
// `Denied` used to mean "sees nothing at all". On an auth-ENABLED server that
// scope covers BOTH an anonymous connection and an authenticated principal
// whose token carries `sub`/`org`/`role` but no `email` (see the `scope_for`
// tests in `server.rs`) — and the session such a connection creates is
// OWNERLESS, so denying it locked the caller out of its own session: empty
// list, resume refused, `send_message` refused, i.e. no product. The .NET twin
// hung CI on exactly this shape and was reverted in #309.
//
// Option B: a conversation that HAS an owner is still owner-checked; one with
// NO owner is readable, as it was before scoping shipped. These pin both halves.

/// One ownerless conversation: created by a `Denied` connection (auth enabled,
/// no principal email) that supplies no `userEmail` either, so its user
/// participant carries no email and nobody owns it.
async fn ownerless() -> (AppState, Arc<InMemoryStorageAdapter>, Created) {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());
    let created = create_session(&state, &storage, &UserScope::Denied, None).await;
    seed_message(&storage, &created.conversation_id, "anonymous question").await;
    (state, storage, created)
}

#[tokio::test]
async fn anonymous_or_emailless_can_use_the_session_it_created() {
    let (state, storage, mine) = ownerless().await;
    let me = UserScope::Denied;

    assert_eq!(
        list_ids(&state, &me).await,
        vec![mine.conversation_id.clone()],
        "an emailless/anonymous principal must see the conversation it created"
    );

    let read = get_messages(&state, &me, &mine.session_id).await;
    assert_eq!(read["type"], "immediate_response", "got: {read}");
    assert_eq!(
        read["data"]["messages"].as_array().expect("messages").len(),
        1
    );

    // Past the ACL gate: the only thing stopping the turn is the absent gateway.
    let events = drive_all(&state, &me, &send_frame(&mine.session_id)).await;
    assert_eq!(
        events[0]["error"]["code"], "LLM_UNAVAILABLE",
        "an emailless principal must be able to send into its own session: {events:?}"
    );

    // And resume binds back to it rather than minting a fresh one.
    let resumed = drive(
        &state,
        &me,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs",
            "agentId": "agent-fixed",
            "conversationId": mine.conversation_id,
        }),
    )
    .await;
    assert_eq!(
        resumed["data"]["conversationId"], mine.conversation_id,
        "got: {resumed}"
    );
    drop(storage);
}

#[tokio::test]
async fn an_emailless_scope_still_cannot_reach_an_owned_session() {
    // The permissive half of Option B is ONLY the ownerless case: an emailless
    // scope matches no non-empty owner, so Alice stays sealed off.
    let (state, storage, a, _b) = two_users().await;
    let me = UserScope::Denied;

    assert!(
        list_ids(&state, &me).await.is_empty(),
        "a connection with no user identity must NOT fall back to the whole org"
    );

    let read = get_messages(&state, &me, &a.session_id).await;
    assert_eq!(read["error"]["code"], "SESSION_NOT_FOUND", "got: {read}");

    let events = drive_all(&state, &me, &send_frame(&a.session_id)).await;
    assert_eq!(events.len(), 1, "no turn may be spawned: {events:?}");
    assert_eq!(events[0]["error"]["code"], "SESSION_NOT_FOUND");

    // Nothing landed in Alice's log.
    let messages = storage
        .list_messages_by_conversation(smooth_operator::adapter::MessageQuery::new(
            &a.conversation_id,
            50,
        ))
        .await
        .expect("list messages")
        .messages;
    assert_eq!(messages.len(), 1, "only alice's own message: {messages:?}");

    // Nor can it resume her conversation — a fresh one is minted, as for an
    // id that never existed.
    let resumed = drive(
        &state,
        &me,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs",
            "agentId": "agent-fixed",
            "conversationId": a.conversation_id,
        }),
    )
    .await;
    assert_ne!(
        resumed["data"]["conversationId"], a.conversation_id,
        "got: {resumed}"
    );
}

#[tokio::test]
async fn an_ownerless_conversation_is_reachable_by_every_scope() {
    // The accepted trade-off. Keying anonymous scope on `sub` instead (Option A)
    // was rejected: Go's anonymous principal uses the literal sub "anonymous"
    // for EVERY visitor, which would pool them and leak their chats to each other.
    let (state, storage, orphan) = ownerless().await;

    // An authenticated user's own conversation, alongside the ownerless one.
    let alice = create_session(&state, &storage, &scoped("alice@example.com"), None).await;
    seed_message(&storage, &alice.conversation_id, "alice's private question").await;

    for scope in [
        UserScope::Denied,
        scoped("alice@example.com"),
        scoped("bob@example.com"),
        UserScope::Unscoped,
    ] {
        assert!(
            list_ids(&state, &scope)
                .await
                .contains(&orphan.conversation_id),
            "ownerless conversations stay reachable for {scope:?}"
        );
        let read = get_messages(&state, &scope, &orphan.session_id).await;
        assert_eq!(read["type"], "immediate_response", "got: {read}");
    }

    // ...while Alice's owned one is still hers alone.
    assert!(!list_ids(&state, &scoped("bob@example.com"))
        .await
        .contains(&alice.conversation_id));
    assert!(!list_ids(&state, &UserScope::Denied)
        .await
        .contains(&alice.conversation_id));
}
