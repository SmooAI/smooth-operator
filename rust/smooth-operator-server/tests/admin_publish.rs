//! Integration tests for `POST /admin/publish` (SMOODEV-1893) — the non-AI
//! realtime publish plug point. Drives the real axum router in-process via
//! `tower::ServiceExt::oneshot` with the real HS256 [`JwtVerifier`], so RBAC is
//! exercised end to end. A sink is attached to the shared backplane so the test
//! asserts the published event is actually delivered to a "connection".

use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt;

use smooth_operator::auth::JwtVerifier;
use smooth_operator::backplane::{LocalSink, Target};
use smooth_operator_server::config::ServerConfig;
use smooth_operator_server::state::AppState;
use smooth_operator_server::{build_state, router};

const SECRET: &[u8] = b"admin-publish-test-secret";
const ORG: &str = "org-acme";

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
        storage: smooth_operator_server::config::StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: Vec::new(),
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

fn state() -> AppState {
    build_state(test_config()).with_auth(Arc::new(JwtVerifier::hs256(SECRET, None, None)))
}

fn token(role: &str) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
    let claims = json!({ "sub": "svc", "org": ORG, "role": role, "exp": exp });
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(SECRET),
    )
    .expect("sign")
}

fn sink() -> (LocalSink, Receiver<Value>) {
    let (tx, rx) = channel::<Value>();
    (
        Arc::new(move |v| {
            let _ = tx.send(v);
        }),
        rx,
    )
}

async fn publish(app: &axum::Router, bearer: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut b = Request::builder().method("POST").uri("/admin/publish");
    if let Some(t) = bearer {
        b = b.header("Authorization", format!("Bearer {t}"));
    }
    let request = b
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(request).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn admin_publish_delivers_to_a_connection_for_the_target() {
    let state = state();
    // A "connection" for session s1 is attached to the shared backplane.
    let (s, rx) = sink();
    state.backplane.attach("c1", s).await;
    state
        .backplane
        .associate("c1", Target::Session("s1".into()))
        .await;
    let app = router(state.clone());

    let (status, body) = publish(
        &app,
        Some(&token("admin")),
        json!({
            "target": { "type": "session", "id": "s1" },
            "event": { "kind": "job_status", "state": "complete" }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["delivered"], 1, "one local delivery");
    assert_eq!(
        rx.try_recv().expect("the connection should receive it"),
        json!({ "kind": "job_status", "state": "complete" })
    );
}

#[tokio::test]
async fn admin_publish_to_unknown_target_delivers_to_nobody() {
    let app = router(state());
    let (status, body) = publish(
        &app,
        Some(&token("admin")),
        json!({ "target": { "type": "user", "id": "nobody" }, "event": { "x": 1 } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["delivered"], 0);
}

#[tokio::test]
async fn non_admin_is_forbidden() {
    let app = router(state());
    let body = json!({ "target": { "type": "session", "id": "s1" }, "event": {} });

    // Curator (role 1) is below the Admin (2) gate.
    let (status, _) = publish(&app, Some(&token("curator")), body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // No token at all.
    let (status, _) = publish(&app, None, body).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
