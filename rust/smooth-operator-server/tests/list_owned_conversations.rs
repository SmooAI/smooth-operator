//! `list_conversations` on a host that requires owned conversations
//! (SMOODEV-3710).
//!
//! Such a host (copilot-ws) can only list what the caller owns, so the handler
//! answers from `StorageAdapter::list_owned_conversation_summaries` instead of
//! scanning every conversation in the org with a participant read and a message
//! read per row. These pin that the fast path returns EXACTLY the rows the scan
//! would: the caller's own non-empty conversations, newest first, capped, with
//! the same title + count — and nothing owned by someone else, nothing ownerless,
//! and nothing at all for an emailless principal.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::domain::{
    Conversation, Direction, Message, MessageContent, Participant, ParticipantType, Platform,
};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler::{self, UserScope};
use smooth_operator_server::state::AppState;

const ORG: &str = "org-alpha";
const ME: &str = "brent@smoo.ai";

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

/// A conversation in `org` named `name`, updated `secs_ago`, whose `user`
/// participant carries `owner` (None = ownerless, the machine-made case).
async fn seed_conversation(
    storage: &InMemoryStorageAdapter,
    org: &str,
    name: &str,
    secs_ago: i64,
    owner: Option<&str>,
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
    storage
        .add_participant(Participant {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: id.clone(),
            organization_id: org.into(),
            participant_type: ParticipantType::User,
            external_id: None,
            internal_id: None,
            browser_fingerprint: None,
            browser_info: None,
            name: "User".into(),
            email: owner.map(str::to_string),
            phone: None,
            crm_contact_id: None,
            metadata_json: None,
            created_at: ts,
            updated_at: ts,
        })
        .await
        .expect("add participant");
    id
}

async fn seed_message(
    storage: &InMemoryStorageAdapter,
    conversation_id: &str,
    inbound: bool,
    text: &str,
) {
    storage
        .append_message(Message {
            id: uuid::Uuid::new_v4().to_string(),
            external_id: None,
            organization_id: Some(ORG.into()),
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

async fn list(state: &AppState, scope: &UserScope, limit: Option<u64>) -> Value {
    let mut frame = json!({ "action": "list_conversations", "requestId": "lc" });
    if let Some(limit) = limit {
        frame["limit"] = json!(limit);
    }
    let (tx, mut rx) = unbounded_channel::<Value>();
    handler::handle_frame(
        state,
        &AccessContext::default().with_organization_id(ORG),
        "conn-test",
        None,
        Some(ORG),
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

fn ids(ev: &Value) -> Vec<String> {
    ev["data"]["conversations"]
        .as_array()
        .unwrap_or_else(|| panic!("conversations array: {ev}"))
        .iter()
        .map(|r| r["conversationId"].as_str().expect("id").to_string())
        .collect()
}

/// Mine (two, newest first), mine-but-empty, someone else's, and a machine's.
struct World {
    state: AppState,
    mine_old: String,
    mine_new: String,
}

async fn world() -> World {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state =
        AppState::new(storage.clone(), base_config()).with_require_owned_conversations(true);

    let mine_old = seed_conversation(&storage, ORG, "Session a", 300, Some(ME)).await;
    seed_message(&storage, &mine_old, true, "how did the pipeline close out?").await;
    seed_message(&storage, &mine_old, false, "Up 12%.").await;

    // A meaningful name wins over the first-inbound preview.
    let mine_new = seed_conversation(&storage, ORG, "Q3 forecast", 10, Some("BRENT@smoo.ai")).await;
    seed_message(&storage, &mine_new, true, "forecast please").await;

    // Owned by me but empty — every drawer open mints one; never listed.
    seed_conversation(&storage, ORG, "Session empty", 5, Some(ME)).await;

    let theirs = seed_conversation(&storage, ORG, "Session t", 1, Some("tara@smoo.ai")).await;
    seed_message(&storage, &theirs, true, "not yours").await;

    let machine = seed_conversation(&storage, ORG, "Session m", 2, None).await;
    seed_message(&storage, &machine, true, "a customer's SMS").await;

    World {
        state,
        mine_old,
        mine_new,
    }
}

#[tokio::test]
async fn lists_only_my_nonempty_conversations_newest_first() {
    let w = world().await;
    let ev = list(&w.state, &UserScope::User(ME.into()), None).await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(
        ids(&ev),
        vec![w.mine_new.clone(), w.mine_old.clone()],
        "got: {ev}"
    );

    let rows = ev["data"]["conversations"].as_array().expect("rows");
    assert_eq!(rows[0]["title"], "Q3 forecast");
    assert_eq!(rows[0]["messageCount"], 1);
    assert_eq!(rows[1]["title"], "how did the pipeline close out?");
    assert_eq!(rows[1]["messageCount"], 2);
}

#[tokio::test]
async fn limit_caps_the_owned_rows() {
    let w = world().await;
    let ev = list(&w.state, &UserScope::User(ME.into()), Some(1)).await;
    assert_eq!(ids(&ev), vec![w.mine_new.clone()], "got: {ev}");
}

#[tokio::test]
async fn an_emailless_principal_lists_nothing() {
    let w = world().await;
    let ev = list(&w.state, &UserScope::Denied, None).await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert!(
        ids(&ev).is_empty(),
        "Denied owns nothing, so it must list nothing: {ev}"
    );
}

#[tokio::test]
async fn matches_the_generic_scan_row_for_row() {
    // The fast path must not change WHAT is listed, only how it is read. The
    // generic scan under the same flag is the oracle.
    let w = world().await;
    let fast = list(&w.state, &UserScope::User(ME.into()), None).await;

    // Same storage, the same flag, but driven through the scan: an Unscoped
    // connection skips the fast path, so filter its rows by what may_read would
    // admit for ME — the conversations ME owns.
    let scan = list(&w.state, &UserScope::Unscoped, None).await;
    let mine: Vec<Value> = scan["data"]["conversations"]
        .as_array()
        .expect("rows")
        .iter()
        .filter(|r| {
            let id = r["conversationId"].as_str().unwrap_or_default();
            id == w.mine_new || id == w.mine_old
        })
        .cloned()
        .collect();
    assert_eq!(fast["data"]["conversations"], Value::Array(mine));
}
