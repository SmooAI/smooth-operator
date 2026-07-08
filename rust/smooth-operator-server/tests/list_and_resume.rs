//! `list_conversations` + resume-by-`conversationId` — the conversation-sidebar
//! / resume substrate (pearl th-d5b446).
//!
//! Drives the real `handler::handle_frame` so the WS protocol contract other
//! clients (daemon PWA, `th code` TUI, chat-widget) build against is proven end
//! to end against in-memory storage:
//!   - `list_conversations` returns only non-empty conversations, most-recent
//!     first, each with a first-inbound title preview + message count;
//!   - `create_conversation_session` with a known `conversationId` RESUMES —
//!     reuses that conversation (no new one minted), and the session's
//!     `get_conversation_messages` sees the prior history;
//!   - an unknown `conversationId` falls back to a fresh conversation.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::domain::{Conversation, Direction, Message, MessageContent, Platform};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler;
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

/// Persist a conversation in `org` with the given `name`, at time offset
/// `secs_ago` (so ordering is deterministic), returning its id.
async fn seed_conversation(
    storage: &InMemoryStorageAdapter,
    org: &str,
    name: &str,
    secs_ago: i64,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now() - chrono::Duration::seconds(secs_ago);
    storage
        .create_conversation(Conversation {
            id: id.clone(),
            platform: Platform::Web,
            name: name.into(),
            organization_id: org.into(),
            idempotency_key: id.clone(),
            metadata_json: None,
            analytics_json: None,
            created_at: ts,
            updated_at: ts,
        })
        .await
        .expect("create conversation");
    id
}

/// Append one text message (direction chosen by `inbound`) to `conversation_id`.
async fn seed_message(
    storage: &InMemoryStorageAdapter,
    org: &str,
    conversation_id: &str,
    inbound: bool,
    text: &str,
) {
    storage
        .append_message(Message {
            id: uuid::Uuid::new_v4().to_string(),
            external_id: None,
            organization_id: Some(org.into()),
            conversation_id: Some(conversation_id.into()),
            direction: if inbound {
                Direction::Inbound
            } else {
                Direction::Outbound
            },
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

/// Drive one frame and return the first emitted event (awaiting the sink).
async fn drive(state: &AppState, auth_org: Option<&str>, frame: &Value) -> Value {
    let (tx, mut rx) = unbounded_channel::<Value>();
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        auth_org,
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

// ---- list_conversations ----------------------------------------------------

#[tokio::test]
async fn list_returns_nonempty_conversations_most_recent_first() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());
    let org = SEED_ORG_ID;

    // Oldest with messages.
    let older = seed_conversation(&storage, org, "Older", 100).await;
    seed_message(&storage, org, &older, true, "First question about billing").await;
    seed_message(&storage, org, &older, false, "Here's the billing answer").await;

    // Newest with messages.
    let newer = seed_conversation(&storage, org, "Newer", 10).await;
    seed_message(&storage, org, &newer, true, "How do I reset my password?").await;

    let ev = drive(
        &state,
        None,
        &json!({ "action": "list_conversations", "requestId": "lc-1" }),
    )
    .await;

    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(ev["status"], 200);
    let convs = ev["data"]["conversations"].as_array().expect("array");
    assert_eq!(convs.len(), 2, "both non-empty convos: {convs:?}");

    // Most-recent-first.
    assert_eq!(convs[0]["conversationId"], newer);
    assert_eq!(convs[1]["conversationId"], older);

    // Title = first inbound preview; counts + ISO updatedAt present.
    assert_eq!(convs[0]["title"], "How do I reset my password?");
    assert_eq!(convs[0]["messageCount"], 1);
    assert_eq!(convs[1]["title"], "First question about billing");
    assert_eq!(convs[1]["messageCount"], 2);
    // updatedAt is ISO-8601 (rfc3339 parses).
    let updated = convs[0]["updatedAt"].as_str().expect("updatedAt string");
    assert!(
        chrono::DateTime::parse_from_rfc3339(updated).is_ok(),
        "updatedAt not ISO-8601: {updated}"
    );
}

#[tokio::test]
async fn list_filters_empty_conversations() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());
    let org = SEED_ORG_ID;

    // Empty (the page-load spam) — must be filtered out.
    seed_conversation(&storage, org, "Empty ghost", 5).await;
    // One with a message survives.
    let live = seed_conversation(&storage, org, "Live", 50).await;
    seed_message(&storage, org, &live, true, "Real message").await;

    let ev = drive(
        &state,
        None,
        &json!({ "action": "list_conversations", "requestId": "lc-2" }),
    )
    .await;

    let convs = ev["data"]["conversations"].as_array().expect("array");
    assert_eq!(convs.len(), 1, "empties filtered: {convs:?}");
    assert_eq!(convs[0]["conversationId"], live);
}

#[tokio::test]
async fn list_title_falls_back_to_name_when_no_inbound() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());
    let org = SEED_ORG_ID;

    // Only an outbound (agent) message — no inbound to preview → fall back to name.
    let conv = seed_conversation(&storage, org, "Named Convo", 5).await;
    seed_message(&storage, org, &conv, false, "Agent-only greeting").await;

    let ev = drive(
        &state,
        None,
        &json!({ "action": "list_conversations", "requestId": "lc-3" }),
    )
    .await;

    let convs = ev["data"]["conversations"].as_array().expect("array");
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0]["title"], "Named Convo");
}

#[tokio::test]
async fn list_truncates_long_first_inbound_preview() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());
    let org = SEED_ORG_ID;

    let long = "x".repeat(200);
    let conv = seed_conversation(&storage, org, "Long", 5).await;
    seed_message(&storage, org, &conv, true, &long).await;

    let ev = drive(
        &state,
        None,
        &json!({ "action": "list_conversations", "requestId": "lc-4" }),
    )
    .await;

    let title = ev["data"]["conversations"][0]["title"]
        .as_str()
        .expect("title");
    // 60 chars + the ellipsis.
    assert_eq!(title.chars().count(), 61, "title: {title}");
    assert!(title.ends_with('…'));
}

#[tokio::test]
async fn list_respects_limit_and_org_scope() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    // Three non-empty convos in org-A.
    for i in 0..3 {
        let c = seed_conversation(&storage, "org-A", &format!("A{i}"), (10 - i) as i64).await;
        seed_message(&storage, "org-A", &c, true, &format!("msg {i}")).await;
    }
    // A convo in a different org must never surface for org-A.
    let other = seed_conversation(&storage, "org-B", "B0", 1).await;
    seed_message(&storage, "org-B", &other, true, "other org message").await;

    let ev = drive(
        &state,
        Some("org-A"),
        &json!({ "action": "list_conversations", "requestId": "lc-5", "limit": 2 }),
    )
    .await;

    let convs = ev["data"]["conversations"].as_array().expect("array");
    assert_eq!(convs.len(), 2, "limit honored: {convs:?}");
    for c in convs {
        assert_ne!(c["conversationId"], other, "cross-org leak");
    }
}

// ---- resume ----------------------------------------------------------------

/// Await the create-session `immediate_response` and return its `conversationId`.
async fn create_session(
    state: &AppState,
    auth_org: Option<&str>,
    frame: &Value,
) -> (String, Value) {
    let ev = drive(state, auth_org, frame).await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(ev["status"], 200, "got: {ev}");
    let cid = ev["data"]["conversationId"]
        .as_str()
        .expect("conversationId")
        .to_string();
    (cid, ev)
}

#[tokio::test]
async fn resume_binds_to_existing_conversation_and_sees_history() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());
    let org = SEED_ORG_ID;

    // Prior conversation with history.
    let existing = seed_conversation(&storage, org, "Prior chat", 100).await;
    seed_message(&storage, org, &existing, true, "Earlier user turn").await;
    seed_message(&storage, org, &existing, false, "Earlier agent reply").await;

    // Resume it.
    let (cid, _ev) = create_session(
        &state,
        None,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-resume",
            "agentId": "agent-1",
            "conversationId": existing,
        }),
    )
    .await;
    assert_eq!(cid, existing, "resume reuses the conversation id");

    // No second conversation was minted — still exactly one for the org.
    let all = storage
        .list_conversations_by_org(org)
        .await
        .expect("list convos");
    assert_eq!(all.len(), 1, "no new conversation on resume: {all:?}");

    // The new session binds to the existing conversation (thread_id = conv id).
    let sessions = storage
        .list_sessions_by_conversation(&existing)
        .await
        .expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].conversation_id, existing);
    assert_eq!(sessions[0].thread_id, existing);
    let session_id = sessions[0].session_id.clone();

    // get_conversation_messages for the resumed session sees the prior history.
    let ev = drive(
        &state,
        None,
        &json!({
            "action": "get_conversation_messages",
            "requestId": "gm-1",
            "sessionId": session_id,
        }),
    )
    .await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    let msgs = ev["data"]["messages"].as_array().expect("messages");
    assert_eq!(msgs.len(), 2, "prior history replayed: {msgs:?}");
}

#[tokio::test]
async fn resume_unknown_conversation_id_falls_back_to_new() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let (cid, _ev) = create_session(
        &state,
        None,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-unknown",
            "agentId": "agent-1",
            "conversationId": "does-not-exist",
        }),
    )
    .await;

    // A fresh conversation was minted (id is NOT the bogus one).
    assert_ne!(cid, "does-not-exist");
    let conv = storage
        .get_conversation(&cid)
        .await
        .expect("get conversation")
        .expect("fresh conversation persisted");
    assert_eq!(conv.id, cid);
}

#[tokio::test]
async fn create_without_conversation_id_mints_fresh_unchanged() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let (cid, _ev) = create_session(
        &state,
        None,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-plain",
            "agentId": "agent-1",
        }),
    )
    .await;

    let conv = storage
        .get_conversation(&cid)
        .await
        .expect("get conversation")
        .expect("conversation persisted");
    assert_eq!(conv.organization_id, SEED_ORG_ID);
}
