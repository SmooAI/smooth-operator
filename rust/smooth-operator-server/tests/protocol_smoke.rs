//! Protocol smoke test — runs in CI with NO credentials.
//!
//! Boots the server in-process on an ephemeral port and drives the wire
//! protocol over a real WebSocket client:
//!   1. `ping` → `pong`
//!   2. `create_conversation_session` → `immediate_response` with a valid
//!      session descriptor
//!   3. `send_message` with no gateway key → a clean `error` event (not a panic
//!      or hang)
//!
//! This proves the server speaks the spec's envelope shapes without ever
//! touching the LLM gateway.

mod common;

use serde_json::json;

use smooth_operator_server::config::ServerConfig;

/// A config with NO gateway key — exactly the CI / no-creds scenario.
fn keyless_config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "claude-haiku-4-5".into(),
        seed_kb: true,
        max_iterations: 4,
        max_tokens: 128,
        storage: smooth_operator_server::config::StorageBackend::Memory,
        widget_auth_strict: false,
    }
}

#[tokio::test]
async fn ping_returns_pong() {
    let url = common::boot(keyless_config()).await;
    let mut client = common::connect(&url).await;

    common::send_json(
        &mut client,
        &json!({ "action": "ping", "requestId": "ping-1" }),
    )
    .await;
    let ev = common::recv_json(&mut client).await;

    assert_eq!(ev["type"], "pong", "expected pong, got: {ev}");
    assert_eq!(ev["requestId"], "ping-1");
    assert!(
        ev["timestamp"].is_i64(),
        "pong must carry a timestamp: {ev}"
    );
    assert_eq!(ev["timestamp"], ev["data"]["timestamp"]);
}

#[tokio::test]
async fn create_session_returns_valid_descriptor() {
    let url = common::boot(keyless_config()).await;
    let mut client = common::connect(&url).await;

    let agent_id = uuid::Uuid::new_v4().to_string();
    common::send_json(
        &mut client,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-1",
            "agentId": agent_id,
            "userName": "Test Visitor",
        }),
    )
    .await;

    let ev = common::recv_json(&mut client).await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(ev["requestId"], "cs-1");
    assert_eq!(ev["status"], 200);

    let data = &ev["data"];
    for field in [
        "sessionId",
        "conversationId",
        "agentId",
        "agentName",
        "userParticipantId",
        "agentParticipantId",
    ] {
        assert!(
            data[field].is_string() && !data[field].as_str().unwrap().is_empty(),
            "session descriptor missing/empty '{field}': {ev}"
        );
    }
    // The agent id we passed is echoed back.
    assert_eq!(data["agentId"], agent_id);
    // sessionId / conversationId are valid UUIDs.
    assert!(uuid::Uuid::parse_str(data["sessionId"].as_str().unwrap()).is_ok());
    assert!(uuid::Uuid::parse_str(data["conversationId"].as_str().unwrap()).is_ok());
}

#[tokio::test]
async fn send_message_without_key_returns_clean_error() {
    let url = common::boot(keyless_config()).await;
    let mut client = common::connect(&url).await;

    // First create a session so the error is about the missing key, not a
    // missing session.
    common::send_json(
        &mut client,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-2",
            "agentId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    let created = common::recv_json(&mut client).await;
    assert_eq!(created["type"], "immediate_response");
    let session_id = created["data"]["sessionId"].as_str().unwrap().to_string();

    // Now send a message — with no gateway key this must error cleanly.
    common::send_json(
        &mut client,
        &json!({
            "action": "send_message",
            "requestId": "sm-1",
            "sessionId": session_id,
            "message": "Hello, are you there?",
        }),
    )
    .await;

    // We may receive the 202 ack OR go straight to error depending on ordering;
    // collect until we hit the error.
    let mut seen = Vec::new();
    let err = common::recv_until(
        &mut client,
        "error",
        &mut seen,
        std::time::Duration::from_secs(10),
    )
    .await;

    assert_eq!(err["type"], "error", "got: {err}");
    assert_eq!(err["requestId"], "sm-1");
    assert_eq!(err["error"]["code"], "LLM_UNAVAILABLE", "got: {err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("SMOOAI_GATEWAY_KEY"),
        "error should mention the missing key: {err}"
    );
    // The nested data.error mirror is present (wire back-compat).
    assert_eq!(err["data"]["error"]["code"], "LLM_UNAVAILABLE");
}

#[tokio::test]
async fn unknown_action_errors_without_dropping_connection() {
    let url = common::boot(keyless_config()).await;
    let mut client = common::connect(&url).await;

    common::send_json(
        &mut client,
        &json!({ "action": "frobnicate", "requestId": "x-1" }),
    )
    .await;
    let err = common::recv_json(&mut client).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["code"], "UNSUPPORTED_ACTION");

    // Connection still alive: a ping still gets a pong.
    common::send_json(
        &mut client,
        &json!({ "action": "ping", "requestId": "x-2" }),
    )
    .await;
    let pong = common::recv_json(&mut client).await;
    assert_eq!(pong["type"], "pong");
    assert_eq!(pong["requestId"], "x-2");
}
