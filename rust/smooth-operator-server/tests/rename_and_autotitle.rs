//! `rename_conversation` + server-side auto-title (pearl th-d5b446).
//!
//! Drives the real `handler` surface so the WS contract (daemon PWA sidebar's
//! rename affordance) and the best-effort auto-titler are proven end to end
//! against in-memory storage:
//!   - `rename_conversation` sets the conversation `name`, sanitizes/rejects
//!     bad input, 404s an unknown id, and the new title surfaces in
//!     `list_conversations`;
//!   - `maybe_auto_title` fires only while a conversation still carries its
//!     default `Session <uuid>` name, sanitizes the model output, and NEVER
//!     clobbers a non-default (manually renamed) name — the gateway call is
//!     served by an in-process mock so no network is touched.

use std::sync::Arc;
use std::time::Duration;

use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use tokio::sync::mpsc::unbounded_channel;

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

/// Persist a conversation in the seed org with the given `name`, returning its id.
async fn seed_conversation(storage: &InMemoryStorageAdapter, name: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now();
    storage
        .create_conversation(Conversation {
            id: id.clone(),
            platform: Platform::Web,
            name: name.into(),
            organization_id: SEED_ORG_ID.into(),
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

async fn seed_inbound(storage: &InMemoryStorageAdapter, conversation_id: &str, text: &str) {
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

/// Drive one frame and return the first emitted event.
async fn drive(state: &AppState, frame: &Value) -> Value {
    let (tx, mut rx) = unbounded_channel::<Value>();
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        None,
        &smooth_operator_server::handler::UserScope::Unscoped,
        &frame.to_string(),
        &tx,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an event should be emitted")
        .expect("sink open")
}

/// Spin an in-process mock gateway that answers `POST /chat/completions` with a
/// canned chat-completion whose content is `title`. Returns the base url (no
/// `/v1`, matching how `generate_title` appends `/chat/completions`).
async fn spawn_mock_gateway(title: &'static str) -> String {
    let app =
        Router::new().route(
            "/chat/completions",
            post(move || async move {
                Json(json!({ "choices": [{ "message": { "content": title } }] }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock gateway");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

// ---- rename_conversation ----------------------------------------------------

#[tokio::test]
async fn rename_sets_name_and_surfaces_in_list() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    // A default-named conversation WITH an inbound message — so absent a rename,
    // `list_conversations` would show the message preview, not the name.
    let conv = seed_conversation(&storage, "Session abc-123").await;
    seed_inbound(&storage, &conv, "please help me reset my password").await;

    let ev = drive(
        &state,
        &json!({ "action": "rename_conversation", "requestId": "r1", "conversationId": conv, "title": "  \"Password reset\"  " }),
    )
    .await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(ev["status"], 200);
    assert_eq!(ev["data"]["title"], "Password reset", "sanitized");
    assert_eq!(ev["data"]["conversationId"], conv);

    // Persisted onto the row.
    let stored = storage.get_conversation(&conv).await.unwrap().unwrap();
    assert_eq!(stored.name, "Password reset");

    // And the sidebar list now surfaces the renamed title (over the message preview).
    let list = drive(
        &state,
        &json!({ "action": "list_conversations", "requestId": "l1" }),
    )
    .await;
    let convs = list["data"]["conversations"].as_array().expect("array");
    let row = convs
        .iter()
        .find(|c| c["conversationId"] == conv)
        .expect("row present");
    assert_eq!(row["title"], "Password reset");
}

#[tokio::test]
async fn rename_rejects_empty_title() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());
    let conv = seed_conversation(&storage, "Session x").await;

    for bad in [json!("   "), json!("\"\""), json!("")] {
        let ev = drive(
            &state,
            &json!({ "action": "rename_conversation", "requestId": "r", "conversationId": conv, "title": bad }),
        )
        .await;
        assert_eq!(ev["type"], "error", "blank title must be rejected: {ev}");
        assert_eq!(ev["error"]["code"], "VALIDATION_ERROR");
    }
    // Name unchanged.
    assert_eq!(
        storage.get_conversation(&conv).await.unwrap().unwrap().name,
        "Session x"
    );
}

#[tokio::test]
async fn rename_unknown_conversation_is_404() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());
    let ev = drive(
        &state,
        &json!({ "action": "rename_conversation", "requestId": "r", "conversationId": "nope", "title": "X" }),
    )
    .await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(ev["error"]["code"], "CONVERSATION_NOT_FOUND");
}

#[tokio::test]
async fn rename_missing_conversation_id_is_validation_error() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());
    let ev = drive(
        &state,
        &json!({ "action": "rename_conversation", "requestId": "r", "title": "X" }),
    )
    .await;
    assert_eq!(ev["error"]["code"], "VALIDATION_ERROR", "got: {ev}");
}

// ---- auto-title -------------------------------------------------------------

#[tokio::test]
async fn auto_title_fires_on_default_named_and_sanitizes() {
    // Mock gateway returns a messy title the sanitizer must clean up.
    let gateway = spawn_mock_gateway("  **\"Password reset help\"**\n").await;
    let mut config = base_config();
    config.gateway_url = gateway;
    config.gateway_key = Some("sk-test".into());

    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), config);
    let conv = seed_conversation(&storage, "Session abc-123").await;

    handler::maybe_auto_title(
        &state,
        &conv,
        "how do I reset my password",
        "Click 'Forgot password'.",
    )
    .await;

    let stored = storage.get_conversation(&conv).await.unwrap().unwrap();
    assert_eq!(
        stored.name, "Password reset help",
        "auto-title stored + sanitized"
    );
}

#[tokio::test]
async fn auto_title_skips_non_default_named_conversation() {
    // Even though the mock WOULD return a title, the guard short-circuits before
    // touching it: a manual rename must never be clobbered.
    let gateway = spawn_mock_gateway("Model chosen title").await;
    let mut config = base_config();
    config.gateway_url = gateway;
    config.gateway_key = Some("sk-test".into());

    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), config);
    let conv = seed_conversation(&storage, "My hand-picked title").await;

    handler::maybe_auto_title(&state, &conv, "user message", "assistant reply").await;

    let stored = storage.get_conversation(&conv).await.unwrap().unwrap();
    assert_eq!(stored.name, "My hand-picked title", "manual name preserved");
}

#[tokio::test]
async fn auto_title_no_gateway_key_leaves_default() {
    // No resolvable key ⇒ no title, default name intact (fail-safe).
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config()); // gateway_key: None
    let conv = seed_conversation(&storage, "Session keep-me").await;

    handler::maybe_auto_title(&state, &conv, "hi", "hello").await;

    assert_eq!(
        storage.get_conversation(&conv).await.unwrap().unwrap().name,
        "Session keep-me"
    );
}
