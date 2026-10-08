//! `list_conversations` paging + search (SMOODEV-3744).
//!
//! The sidebar used to return only the newest `limit` rows with no way to reach
//! older ones, and a client-side search could only filter what it had loaded.
//! These pin the keyset `cursor` / `nextCursor` and the server-side `query` on
//! BOTH listing paths — the owned fast path a `require_owned_conversations`
//! host takes, and the generic scan everyone else takes:
//!
//! - pages are disjoint and, together, exactly the unpaged listing;
//! - ties on `updatedAt` are broken by id, so equal timestamps neither repeat
//!   nor vanish across a page boundary;
//! - a conversation bumped mid-paging is never returned twice, and every
//!   untouched one is still returned (the bumped one moves above the cursor —
//!   documented keyset behaviour);
//! - a search narrows the caller's scope and never widens it: another member's
//!   chat and an ownerless (widget/SMS) chat that match are still excluded.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{ConversationUpdate, StorageAdapter};
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

/// A non-empty conversation updated at `ts`, owned by `owner` (None = the
/// machine-made, ownerless case), whose first inbound message is `first`.
async fn seed(
    storage: &InMemoryStorageAdapter,
    name: &str,
    ts: chrono::DateTime<chrono::Utc>,
    owner: Option<&str>,
    first: &str,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    storage
        .create_conversation(Conversation {
            id: id.clone(),
            platform: Platform::Web,
            name: name.into(),
            organization_id: ORG.into(),
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
            organization_id: ORG.into(),
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
    storage
        .append_message(Message {
            id: uuid::Uuid::new_v4().to_string(),
            external_id: None,
            organization_id: Some(ORG.into()),
            conversation_id: Some(id.clone()),
            direction: Direction::Inbound,
            content: MessageContent::from_text(first),
            from: None,
            to: None,
            metadata_json: None,
            analytics_json: None,
            created_at: ts,
            updated_at: None,
        })
        .await
        .expect("append message");
    id
}

fn ago(secs: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() - chrono::Duration::seconds(secs)
}

async fn list(state: &AppState, scope: &UserScope, args: Value) -> Value {
    let mut frame = json!({ "action": "list_conversations", "requestId": "lc" });
    for (k, v) in args.as_object().expect("args object") {
        frame[k] = v.clone();
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

/// Page through everything with `limit`, returning each page's ids.
async fn all_pages(
    state: &AppState,
    scope: &UserScope,
    limit: u64,
    query: Option<&str>,
) -> Vec<Vec<String>> {
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut args = json!({ "limit": limit });
        if let Some(c) = &cursor {
            args["cursor"] = json!(c);
        }
        if let Some(q) = query {
            args["query"] = json!(q);
        }
        let ev = list(state, scope, args).await;
        assert_eq!(ev["type"], "immediate_response", "got: {ev}");
        pages.push(ids(&ev));
        let has_more = ev["data"]["hasMore"].as_bool().expect("hasMore");
        match ev["data"]["nextCursor"].as_str() {
            Some(next) => {
                assert!(has_more, "a nextCursor implies hasMore: {ev}");
                cursor = Some(next.to_string());
            }
            None => {
                assert!(!has_more, "no nextCursor means no more: {ev}");
                assert!(
                    ev["data"]["nextCursor"].is_null(),
                    "nextCursor is explicit null: {ev}"
                );
                break;
            }
        }
        assert!(pages.len() < 50, "paging never terminated");
    }
    pages
}

/// Two listing paths over one world: the owned fast path (`require_owned`) and
/// the generic scan. The scan is exercised with a `User` scope on a host
/// WITHOUT the flag, so ownerless rows are visible there (that's the legacy
/// widget behaviour) — which the tests account for.
struct World {
    storage: Arc<InMemoryStorageAdapter>,
    /// Mine, newest first.
    mine: Vec<String>,
    /// The one of mine whose first message mentions Acme.
    acme: String,
    theirs: String,
    machine: String,
}

async fn world() -> World {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let mut mine = Vec::new();
    let mut acme = String::new();
    // Seven of mine; three of them share one timestamp to force the id tiebreak
    // across a page boundary at limit 3.
    let tied = ago(400);
    for (i, ts) in [ago(10), ago(20), tied, tied, tied, ago(500), ago(600)]
        .into_iter()
        .enumerate()
    {
        let first = if i == 5 {
            "Where is the Acme invoice?"
        } else {
            "status update"
        };
        let id = seed(&storage, &format!("Session {i}"), ts, Some(ME), first).await;
        if i == 5 {
            acme = id.clone();
        }
        mine.push(id);
    }
    let theirs = seed(
        &storage,
        "Session t",
        ago(15),
        Some("tara@smoo.ai"),
        "acme renewal",
    )
    .await;
    let machine = seed(&storage, "Session m", ago(25), None, "ACME widget chat").await;
    // Expected order: newest first, ties by id descending.
    let mut keyed: Vec<(chrono::DateTime<chrono::Utc>, String)> = Vec::new();
    for id in &mine {
        let c = storage.get_conversation(id).await.unwrap().unwrap();
        keyed.push((c.updated_at, c.id));
    }
    keyed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    World {
        storage,
        mine: keyed.into_iter().map(|(_, id)| id).collect(),
        acme,
        theirs,
        machine,
    }
}

fn owned_state(w: &World) -> AppState {
    AppState::new(w.storage.clone(), base_config()).with_require_owned_conversations(true)
}

fn scan_state(w: &World) -> AppState {
    AppState::new(w.storage.clone(), base_config())
}

fn me() -> UserScope {
    UserScope::User(ME.into())
}

#[tokio::test]
async fn owned_pages_are_disjoint_and_complete() {
    let w = world().await;
    let pages = all_pages(&owned_state(&w), &me(), 3, None).await;
    assert_eq!(
        pages.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![3, 3, 1]
    );
    assert_eq!(
        pages.concat(),
        w.mine,
        "pages concatenate to the full listing, in order"
    );
}

#[tokio::test]
async fn scan_pages_are_disjoint_and_complete() {
    let w = world().await;
    let unpaged = list(&scan_state(&w), &me(), json!({ "limit": 100 })).await;
    let pages = all_pages(&scan_state(&w), &me(), 2, None).await;
    assert_eq!(pages.concat(), ids(&unpaged));
    // The scan (no require-owned flag) sees ownerless rows but never another member's.
    assert!(pages.concat().contains(&w.machine));
    assert!(!pages.concat().contains(&w.theirs));
}

#[tokio::test]
async fn a_page_the_size_of_the_rest_reports_no_more() {
    let w = world().await;
    let ev = list(&owned_state(&w), &me(), json!({ "limit": 7 })).await;
    assert_eq!(ids(&ev), w.mine);
    assert_eq!(ev["data"]["hasMore"], false, "got: {ev}");
    assert!(ev["data"]["nextCursor"].is_null(), "got: {ev}");
}

#[tokio::test]
async fn no_cursor_is_the_old_first_page() {
    let w = world().await;
    let ev = list(&owned_state(&w), &me(), json!({})).await;
    assert_eq!(ids(&ev), w.mine, "default limit 50 returns every owned row");
    let ev = list(&owned_state(&w), &me(), json!({ "limit": 2, "cursor": "" })).await;
    assert_eq!(
        ids(&ev),
        w.mine[..2].to_vec(),
        "a blank cursor is no cursor"
    );
}

/// Bump one row on the NEXT page and one on the page already read, between
/// pages. Neither may be returned twice; every untouched row must still come
/// back; the bumped next-page row moves above the cursor and is not returned by
/// the remaining pages (it heads a fresh first page instead).
async fn bump_between_pages(state: &AppState, w: &World) {
    let first = list(state, &me(), json!({ "limit": 3 })).await;
    let page1 = ids(&first);
    let cursor = first["data"]["nextCursor"]
        .as_str()
        .expect("more")
        .to_string();

    let bumped_ahead = w.mine[4].clone(); // would have been on page 2
    let bumped_behind = page1[1].clone(); // already returned
    for id in [&bumped_ahead, &bumped_behind] {
        tokio::time::sleep(Duration::from_millis(2)).await;
        w.storage
            .update_conversation(id, ConversationUpdate::default())
            .await
            .expect("bump updated_at");
    }

    let mut rest = Vec::new();
    let mut cursor = Some(cursor);
    while let Some(c) = cursor {
        let ev = list(state, &me(), json!({ "limit": 3, "cursor": c })).await;
        rest.extend(ids(&ev));
        cursor = ev["data"]["nextCursor"].as_str().map(str::to_string);
    }

    let mut seen = page1.clone();
    seen.extend(rest.iter().cloned());
    let mut dedup = seen.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), seen.len(), "no row returned twice: {seen:?}");
    for id in &w.mine {
        if *id != bumped_ahead {
            assert!(
                seen.contains(id),
                "untouched row {id} was dropped: {seen:?}"
            );
        }
    }
    assert!(
        !rest.contains(&bumped_ahead),
        "a row bumped above the cursor is not on later pages"
    );

    let fresh = list(state, &me(), json!({ "limit": 1 })).await;
    assert_eq!(
        ids(&fresh),
        vec![bumped_behind],
        "the latest bump heads a fresh first page"
    );
}

#[tokio::test]
async fn owned_paging_survives_concurrent_updates() {
    let w = world().await;
    bump_between_pages(&owned_state(&w), &w).await;
}

#[tokio::test]
async fn scan_paging_survives_concurrent_updates() {
    // The scan also lists the ownerless row, so assert only about `w.mine`.
    let w = world().await;
    let state = scan_state(&w);
    let first = list(&state, &me(), json!({ "limit": 3 })).await;
    let cursor = first["data"]["nextCursor"]
        .as_str()
        .expect("more")
        .to_string();
    w.storage
        .update_conversation(
            first["data"]["conversations"][1]["conversationId"]
                .as_str()
                .unwrap(),
            ConversationUpdate::default(),
        )
        .await
        .unwrap();
    let rest = all_pages_from(&state, cursor).await;
    let mut seen = ids(&first);
    seen.extend(rest);
    let mut dedup = seen.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), seen.len(), "no row returned twice: {seen:?}");
    for id in &w.mine {
        assert!(seen.contains(id), "row {id} was dropped: {seen:?}");
    }
}

async fn all_pages_from(state: &AppState, cursor: String) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = Some(cursor);
    while let Some(c) = cursor {
        let ev = list(state, &me(), json!({ "limit": 3, "cursor": c })).await;
        out.extend(ids(&ev));
        cursor = ev["data"]["nextCursor"].as_str().map(str::to_string);
    }
    out
}

#[tokio::test]
async fn owned_search_matches_my_titles_and_never_widens_scope() {
    let w = world().await;
    let state = owned_state(&w);
    // "acme" matches my first message, Tara's chat and the widget chat; only mine is listed.
    let ev = list(&state, &me(), json!({ "query": "  AcMe " })).await;
    assert_eq!(ids(&ev), vec![w.acme.clone()], "got: {ev}");
    assert_eq!(
        ev["data"]["conversations"][0]["title"],
        "Where is the Acme invoice?"
    );
    assert_eq!(ev["data"]["hasMore"], false);
}

#[tokio::test]
async fn scan_search_never_returns_another_members_chat() {
    let w = world().await;
    let ev = list(&scan_state(&w), &me(), json!({ "query": "acme" })).await;
    let got = ids(&ev);
    assert!(got.contains(&w.acme), "got: {ev}");
    assert!(
        !got.contains(&w.theirs),
        "another member's matching chat leaked: {ev}"
    );
}

#[tokio::test]
async fn search_matches_a_meaningful_name_but_not_the_default_placeholder() {
    let w = world().await;
    w.storage
        .update_conversation(
            &w.mine[6],
            ConversationUpdate {
                name: Some("Q3 Forecast".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let state = owned_state(&w);
    let ev = list(&state, &me(), json!({ "query": "forecast" })).await;
    assert_eq!(ids(&ev), vec![w.mine[6].clone()], "got: {ev}");
    // Every default-named row starts "Session …"; that placeholder is not a title.
    let ev = list(&state, &me(), json!({ "query": "session" })).await;
    assert!(ids(&ev).is_empty(), "got: {ev}");
}

#[tokio::test]
async fn search_pages_with_a_cursor() {
    let w = world().await;
    let pages = all_pages(&owned_state(&w), &me(), 2, Some("status")).await;
    let expected: Vec<String> = w.mine.iter().filter(|id| **id != w.acme).cloned().collect();
    assert_eq!(
        pages.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2, 2, 2]
    );
    assert_eq!(
        pages.concat(),
        expected,
        "the six 'status update' rows, in order, once each"
    );
}

#[tokio::test]
async fn an_unknown_cursor_is_a_validation_error() {
    let w = world().await;
    for cursor in ["not-a-cursor", "bm9waXBl"] {
        let ev = list(&owned_state(&w), &me(), json!({ "cursor": cursor })).await;
        assert_eq!(ev["type"], "error", "got: {ev}");
        assert_eq!(ev["data"]["error"]["code"], "VALIDATION_ERROR", "got: {ev}");
    }
}

#[tokio::test]
async fn an_emailless_principal_pages_nothing() {
    let w = world().await;
    let ev = list(
        &owned_state(&w),
        &UserScope::Denied,
        json!({ "query": "acme" }),
    )
    .await;
    assert!(ids(&ev).is_empty(), "got: {ev}");
    assert_eq!(ev["data"]["hasMore"], false);
}

#[tokio::test]
async fn a_limit_over_the_maximum_is_clamped_not_rejected() {
    // 205 of mine: a pre-paging client asking for 1000 gets the 200-row
    // maximum and a cursor for the rest, not an error.
    let storage = Arc::new(InMemoryStorageAdapter::new());
    for i in 0..205 {
        seed(
            &storage,
            &format!("Session {i}"),
            ago(i),
            Some(ME),
            "status update",
        )
        .await;
    }
    for state in [
        AppState::new(storage.clone(), base_config()).with_require_owned_conversations(true),
        AppState::new(storage.clone(), base_config()),
    ] {
        let ev = list(&state, &me(), json!({ "limit": 1000 })).await;
        assert_eq!(ev["type"], "immediate_response", "got: {ev}");
        assert_eq!(ids(&ev).len(), 200);
        assert_eq!(ev["data"]["hasMore"], true);
        let rest = list(
            &state,
            &me(),
            json!({ "limit": 1000, "cursor": ev["data"]["nextCursor"] }),
        )
        .await;
        assert_eq!(ids(&rest).len(), 5);
        assert_eq!(rest["data"]["hasMore"], false);
    }
}
