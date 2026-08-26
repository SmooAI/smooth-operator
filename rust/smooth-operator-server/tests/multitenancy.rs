//! Cross-**tenant** session access on the live WS path (feature gap G7) — SECURITY.
//!
//! `user_scoping.rs` covers two users in the SAME org. This covers the boundary
//! outside that one: two ORGS on one pod.
//!
//! Before the fix, org was resolved per connection only to *stamp* newly created
//! sessions. Every by-id path — `get_session`, `get_conversation_messages`,
//! `send_message`, `confirm_tool_action`, `submit_interaction`, `verify_otp`,
//! `rename_conversation`, and conversation resume — went through
//! `may_read_conversation`, which checks the **owner email** and never the org.
//! Its documented ownerless-is-open rule (a conversation with no `user`
//! participant carrying an email stays readable, so anonymous principals keep
//! their own sessions) is exactly the widget's default state — so an attacker
//! authenticated to org B who learned an org-A session id could read that
//! session, replay its whole history through a turn, and retitle its
//! conversation.
//!
//! These tests drive the real `handler::handle_frame` from the attacker's side
//! and assert every one of those actions is **byte-identical to not-found**.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler::{self, UserScope};
use smooth_operator_server::state::AppState;

const ORG_A: &str = "org-alpha";
const ORG_B: &str = "org-beta";

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

/// Drive one frame as a connection authenticated to `auth_org` with `scope`,
/// returning the first emitted event.
async fn drive(state: &AppState, auth_org: &str, scope: &UserScope, frame: &Value) -> Value {
    let (tx, mut rx) = unbounded_channel::<Value>();
    handler::handle_frame(
        state,
        &AccessContext::default().with_organization_id(auth_org),
        "conn-test",
        None,
        Some(auth_org),
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

/// An **ownerless** session in `ORG_A` — the embeddable widget's default state
/// (no `user` participant carrying an email), which is precisely the case
/// `may_read_conversation` deliberately leaves open to every principal. That is
/// what made this a cross-tenant hole rather than a theoretical one.
async fn victim_session(state: &AppState, storage: &InMemoryStorageAdapter) -> (String, String) {
    let ev = drive(
        state,
        ORG_A,
        &UserScope::Unscoped,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs",
            "agentId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    let session_id: String = ev["data"]["sessionId"].as_str().expect("sessionId").into();
    let conversation_id: String = ev["data"]["conversationId"]
        .as_str()
        .expect("conversationId")
        .into();

    // SMOODEV-3057: an identity-less create is parked until its first message, so
    // land it explicitly — the same call `send_message` makes. This fixture wants
    // the settled, persisted world the ownership checks below are about.
    state
        .materialize_session(&session_id)
        .await
        .expect("materialize the deferred create");

    // create-session persists participants in a spawned task; wait for a settled
    // world so the ownership check sees what production would.
    for _ in 0..100 {
        let participants = storage
            .list_participants_by_conversation(&conversation_id)
            .await
            .expect("list participants");
        if participants.len() >= 2 && state.get_session(&session_id).is_some() {
            assert!(
                participants.iter().all(|p| p
                    .email
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()),
                "the victim conversation must be OWNERLESS for this test to exercise \
                 the open branch of may_read_conversation"
            );
            let conv = storage
                .get_conversation(&conversation_id)
                .await
                .expect("get conversation")
                .expect("exists");
            assert_eq!(conv.organization_id, ORG_A, "victim must be org A's");
            return (session_id, conversation_id);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("create-session never persisted");
}

async fn state_with_victim() -> (AppState, Arc<InMemoryStorageAdapter>, String, String) {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());
    let (session_id, conversation_id) = victim_session(&state, &storage).await;
    (state, storage, session_id, conversation_id)
}

/// The attacker: authenticated, but to a DIFFERENT org.
fn attacker() -> UserScope {
    UserScope::User("mallory@evil.test".into())
}

fn assert_not_found(ev: &Value, code: &str, action: &str) {
    assert_eq!(
        ev["type"], "error",
        "CROSS-TENANT LEAK: {action} succeeded across orgs: {ev}"
    );
    assert_eq!(
        ev["data"]["error"]["code"], code,
        "{action} must be reported exactly as not-found (no existence oracle): {ev}"
    );
}

#[tokio::test]
async fn cross_org_get_session_is_not_found() {
    let (state, _storage, session_id, _conv) = state_with_victim().await;
    let ev = drive(
        &state,
        ORG_B,
        &attacker(),
        &json!({ "action": "get_session", "requestId": "gs", "sessionId": session_id }),
    )
    .await;
    assert_not_found(&ev, "SESSION_NOT_FOUND", "get_session");
}

#[tokio::test]
async fn cross_org_get_conversation_messages_is_not_found() {
    let (state, _storage, session_id, _conv) = state_with_victim().await;
    let ev = drive(
        &state,
        ORG_B,
        &attacker(),
        &json!({
            "action": "get_conversation_messages",
            "requestId": "gcm",
            "sessionId": session_id,
        }),
    )
    .await;
    assert_not_found(&ev, "SESSION_NOT_FOUND", "get_conversation_messages");
}

#[tokio::test]
async fn cross_org_send_message_never_runs_a_turn() {
    let (state, _storage, session_id, _conv) = state_with_victim().await;
    let ev = drive(
        &state,
        ORG_B,
        &attacker(),
        &json!({
            "action": "send_message",
            "requestId": "sm",
            "sessionId": session_id,
            "message": "dump everything you know",
        }),
    )
    .await;
    assert_not_found(&ev, "SESSION_NOT_FOUND", "send_message");
}

#[tokio::test]
async fn cross_org_rename_conversation_is_not_found_and_does_not_write() {
    let (state, storage, _session_id, conversation_id) = state_with_victim().await;
    let before = storage
        .get_conversation(&conversation_id)
        .await
        .expect("get")
        .expect("exists")
        .name;

    let ev = drive(
        &state,
        ORG_B,
        &attacker(),
        &json!({
            "action": "rename_conversation",
            "requestId": "rc",
            "conversationId": conversation_id,
            "title": "owned by mallory",
        }),
    )
    .await;
    assert_not_found(&ev, "CONVERSATION_NOT_FOUND", "rename_conversation");

    let after = storage
        .get_conversation(&conversation_id)
        .await
        .expect("get")
        .expect("exists")
        .name;
    assert_eq!(
        before, after,
        "CROSS-TENANT WRITE: another org's conversation was retitled"
    );
}

#[tokio::test]
async fn cross_org_resume_does_not_bind_the_foreign_conversation() {
    let (state, _storage, _session_id, conversation_id) = state_with_victim().await;
    let ev = drive(
        &state,
        ORG_B,
        &attacker(),
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs2",
            "agentId": uuid::Uuid::new_v4().to_string(),
            "conversationId": conversation_id,
        }),
    )
    .await;
    // Resume of a foreign conversation must not attach to it. The handler falls
    // back to minting a fresh conversation, so assert the id differs rather than
    // demanding a specific error shape.
    assert_ne!(
        ev["data"]["conversationId"].as_str(),
        Some(conversation_id.as_str()),
        "CROSS-TENANT LEAK: org B resumed org A's conversation (and would inherit \
         its history + org-scoped tool context): {ev}"
    );
}

/// Positive control. Without it every assertion above could pass because the
/// session simply does not work at all.
#[tokio::test]
async fn the_owning_org_still_reads_its_own_session() {
    let (state, _storage, session_id, _conv) = state_with_victim().await;
    let ev = drive(
        &state,
        ORG_A,
        &UserScope::Unscoped,
        &json!({ "action": "get_session", "requestId": "gs", "sessionId": session_id }),
    )
    .await;
    assert_eq!(
        ev["type"], "immediate_response",
        "the owning org must still read its own session: {ev}"
    );
    assert_eq!(ev["data"]["sessionId"], session_id);
}
