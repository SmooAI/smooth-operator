//! Embeddable-widget auth enforcement (SMOODEV-1878).
//!
//! Boots the server with a [`StaticWidgetAuth`] provider holding one policied
//! agent, then drives `create_conversation_session` over a real WebSocket from
//! different `Origin`s to prove the allowlist is enforced — and that agents with
//! no policy are unaffected in the default (permissive) mode.

mod common;

use std::sync::Arc;

use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use smooth_operator::widget_auth::StaticWidgetAuth;
use smooth_operator_server::config::ServerConfig;
use smooth_operator_server::server::{build_state, router};

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

/// One policied agent: only `https://good.example.com` may embed `agent-embed`.
const POLICY: &str = r#"{ "agent-embed": { "allowed_origins": ["https://good.example.com"] } }"#;

async fn boot_with_widget_auth(json_policy: &str) -> String {
    let state = build_state(keyless_config()).with_widget_auth(Arc::new(
        StaticWidgetAuth::from_json(json_policy).expect("policy json"),
    ));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("ws://{addr}/ws")
}

async fn connect_with_origin(url: &str, origin: &str) -> common::Client {
    let mut req = url.into_client_request().expect("request");
    req.headers_mut()
        .insert("origin", origin.parse().expect("origin header"));
    let (ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .expect("connect ws");
    ws
}

#[tokio::test]
async fn disallowed_origin_is_rejected() {
    let url = boot_with_widget_auth(POLICY).await;
    let mut c = connect_with_origin(&url, "https://evil.example.com").await;
    common::send_json(&mut c, &json!({ "action": "create_conversation_session", "requestId": "cs-1", "agentId": "agent-embed" })).await;
    let ev = common::recv_json(&mut c).await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(ev["error"]["code"], "ORIGIN_NOT_ALLOWED", "got: {ev}");
}

#[tokio::test]
async fn missing_origin_is_rejected_for_policied_agent() {
    // No Origin header at all → fail closed for a policied agent.
    let url = boot_with_widget_auth(POLICY).await;
    let mut c = common::connect(&url).await;
    common::send_json(&mut c, &json!({ "action": "create_conversation_session", "requestId": "cs-2", "agentId": "agent-embed" })).await;
    let ev = common::recv_json(&mut c).await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(ev["error"]["code"], "ORIGIN_NOT_ALLOWED", "got: {ev}");
}

#[tokio::test]
async fn allowed_origin_succeeds() {
    let url = boot_with_widget_auth(POLICY).await;
    let mut c = connect_with_origin(&url, "https://good.example.com").await;
    common::send_json(&mut c, &json!({ "action": "create_conversation_session", "requestId": "cs-3", "agentId": "agent-embed" })).await;
    let ev = common::recv_json(&mut c).await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(ev["status"], 200);
}

#[tokio::test]
async fn unpolicied_agent_unaffected_in_permissive_mode() {
    // An agent NOT in the policy map → no enforcement (default, non-strict).
    let url = boot_with_widget_auth(POLICY).await;
    let mut c = connect_with_origin(&url, "https://anywhere.example.com").await;
    common::send_json(&mut c, &json!({ "action": "create_conversation_session", "requestId": "cs-4", "agentId": "some-other-agent" })).await;
    let ev = common::recv_json(&mut c).await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
}
