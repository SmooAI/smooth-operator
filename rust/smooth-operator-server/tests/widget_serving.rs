//! Integration tests for the local-flavor widget-serving routes.
//!
//! The local deployment flavor opts into serving the official **Aurora Glass**
//! widget (`@smooai/chat-widget`): the host page at `/` (with the auth token
//! injected, same-origin/loopback) and the vendored bundle at
//! `/chat-widget.iife.js`. K8s/Lambda flavors never mount these routes. Drives
//! the real axum router in-process via `tower::ServiceExt::oneshot` — no network.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::{build_state, router};

/// A minimal in-memory config (no LLM key) — the widget routes don't touch the engine.
fn test_config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.test/v1".into(),
        gateway_key: None,
        model: "m".into(),
        seed_kb: false,
        max_iterations: 4,
        max_tokens: 128,
        storage: StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: Vec::new(),
    }
}

/// GET `path`; return `(status, content_type, text body)`.
async fn get(app: &axum::Router, path: &str) -> (StatusCode, Option<String>, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
async fn serves_aurora_glass_bundle() {
    let state = build_state(test_config()).with_widget(Some("tok-abc".into()));
    let app = router(state);

    let (status, content_type, body) = get(&app, "/chat-widget.iife.js").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        content_type.as_deref(),
        Some("application/javascript; charset=utf-8")
    );
    // The vendored bundle is @smooai/chat-widget (Aurora Glass): it registers the
    // <smooth-agent-chat> custom element, exposes the SmoothAgentChat global, and
    // carries the inlined protocol client. These guard against a broken or
    // wrong-package re-vendor (see assets/README.md).
    assert!(
        body.contains("smooth-agent-chat"),
        "bundle missing the <smooth-agent-chat> custom element tag"
    );
    assert!(
        body.contains("SmoothAgentChat"),
        "bundle missing the SmoothAgentChat global"
    );
    assert!(
        body.contains("send_message"),
        "bundle missing the inlined protocol client"
    );
}

#[tokio::test]
async fn host_page_injects_token() {
    let state = build_state(test_config()).with_widget(Some("tok-xyz".into()));
    let app = router(state);

    let (status, _ct, body) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    // The `__SMOOTH_LOCAL_TOKEN__` placeholder must be substituted with the
    // JSON-encoded token, and the page must mount the widget element.
    assert!(
        !body.contains("__SMOOTH_LOCAL_TOKEN__"),
        "token placeholder was not substituted"
    );
    assert!(
        body.contains("\"tok-xyz\""),
        "injected token not found in the host page"
    );
    assert!(
        body.contains("smooth-agent-chat"),
        "host page does not mount the widget element"
    );
}

#[tokio::test]
async fn widget_routes_are_off_by_default() {
    // Without `.with_widget(...)` the flavor does not opt in, so neither route is
    // mounted — K8s/Lambda never serve the widget.
    let app = router(build_state(test_config()));

    let (bundle_status, _ct, _body) = get(&app, "/chat-widget.iife.js").await;
    assert_eq!(
        bundle_status,
        StatusCode::NOT_FOUND,
        "bundle must not be served unless opted in"
    );

    let (index_status, _ct, _body) = get(&app, "/").await;
    assert_eq!(
        index_status,
        StatusCode::NOT_FOUND,
        "host page must not be served unless opted in"
    );
}
