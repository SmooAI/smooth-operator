//! Integration tests for the admin **write** API (Phase 12, increment 3).
//!
//! Connector-config CRUD, settings, and the trigger-an-indexing-run endpoint.
//! Drives the real axum router in-process via `tower::ServiceExt::oneshot` — no
//! live gateway or network. Auth runs through the **real** [`JwtVerifier`] with
//! HS256 tokens signed in-test, so the RBAC gates and org-scoping are exercised
//! end to end.
//!
//! The `/index` endpoint is exercised **offline** two ways:
//!   1. a `web`/`file`-kind connector pointed at a local temp file (no network),
//!      which runs a real `IndexingService::run_once` and lands a run in the
//!      `IndexingStore`; and
//!   2. a `github`-kind connector with an **unresolvable `auth_ref`**, which must
//!      return a clean 400 *before* any GitHub call (no panic, no network).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt;

use smooth_operator::auth::JwtVerifier;
use smooth_operator_server::config::ServerConfig;
use smooth_operator_server::{build_state, router};

const SECRET: &[u8] = b"admin-write-test-secret";
const ORG: &str = "org-acme";
const OTHER_ORG: &str = "org-other";

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
    }
}

/// App with the real HS256 JwtVerifier (default in-memory stores).
fn app() -> axum::Router {
    let state =
        build_state(test_config()).with_auth(Arc::new(JwtVerifier::hs256(SECRET, None, None)));
    router(state)
}

/// Sign an HS256 token for `(user, role)` in `org`.
fn token_in(org: &str, user: &str, role: &str) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
    let claims = json!({ "sub": user, "org": org, "role": role, "exp": exp });
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(SECRET),
    )
    .expect("sign")
}

fn token(user: &str, role: &str) -> String {
    token_in(ORG, user, role)
}

/// Issue an HTTP request with method/path/optional-bearer/optional-json-body.
async fn req(
    app: &axum::Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = bearer {
        b = b.header("Authorization", format!("Bearer {t}"));
    }
    let request = if let Some(json) = body {
        b.header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&json).unwrap()))
            .unwrap()
    } else {
        b.body(Body::empty()).unwrap()
    };
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

// ---------------------------------------------------------------------------
// Connector CRUD + RBAC
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_creates_connector_then_it_is_listed() {
    let app = app();
    let create = json!({
        "name": "Docs repo",
        "kind": "web",
        "config": { "url": "https://example.test/page" },
        "enabled": true
    });
    let (status, body) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("a", "admin")),
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create: {body:?}");
    let id = body["connector"]["id"].as_str().expect("id").to_string();
    assert_eq!(body["connector"]["name"], "Docs repo");
    assert_eq!(body["connector"]["kind"], "web");

    // Curator can list and sees it (org-scoped).
    let (status, body) = req(
        &app,
        "GET",
        "/admin/connectors",
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let conns = body["connectors"].as_array().expect("array");
    assert_eq!(conns.len(), 1);
    assert_eq!(conns[0]["id"], id);

    // GET by id (Curator).
    let (status, body) = req(
        &app,
        "GET",
        &format!("/admin/connectors/{id}"),
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["connector"]["id"], id);
}

#[tokio::test]
async fn basic_is_forbidden_to_create_update_delete() {
    let app = app();
    let create = json!({ "name": "x", "kind": "web", "config": { "url": "https://e.test" } });

    let (status, body) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("u", "basic")),
        Some(create.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "FORBIDDEN");

    let (status, _) = req(
        &app,
        "PUT",
        "/admin/connectors/whatever",
        Some(&token("u", "basic")),
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = req(
        &app,
        "DELETE",
        "/admin/connectors/whatever",
        Some(&token("u", "basic")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn curator_can_list_but_not_create() {
    let app = app();
    // Curator CAN list (read).
    let (status, _) = req(
        &app,
        "GET",
        "/admin/connectors",
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Curator CANNOT create (Admin-only write).
    let create = json!({ "name": "x", "kind": "web", "config": { "url": "https://e.test" } });
    let (status, _) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("c", "curator")),
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cross_org_get_and_delete_are_404() {
    let app = app();
    // Admin in ORG creates a connector.
    let create = json!({ "name": "x", "kind": "web", "config": { "url": "https://e.test" } });
    let (status, body) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("a", "admin")),
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["connector"]["id"].as_str().unwrap().to_string();

    // Admin in OTHER_ORG cannot see it (404, not 403 — don't leak existence).
    let other = token_in(OTHER_ORG, "a2", "admin");
    let (status, b) = req(
        &app,
        "GET",
        &format!("/admin/connectors/{id}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b:?}");

    // Cross-org delete is also a 404.
    let (status, _) = req(
        &app,
        "DELETE",
        &format!("/admin/connectors/{id}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The original org still has it.
    let (status, body) = req(
        &app,
        "GET",
        "/admin/connectors",
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["connectors"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn put_updates_and_delete_removes() {
    let app = app();
    let create = json!({ "name": "before", "kind": "web", "config": { "url": "https://e.test" }, "enabled": true });
    let (_, body) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("a", "admin")),
        Some(create),
    )
    .await;
    let id = body["connector"]["id"].as_str().unwrap().to_string();

    // PUT updates name + enabled.
    let update = json!({ "name": "after", "kind": "web", "config": { "url": "https://e2.test" }, "enabled": false });
    let (status, body) = req(
        &app,
        "PUT",
        &format!("/admin/connectors/{id}"),
        Some(&token("a", "admin")),
        Some(update),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["connector"]["name"], "after");
    assert_eq!(body["connector"]["enabled"], false);
    assert_eq!(body["connector"]["id"], id, "id preserved across update");

    // GET reflects it.
    let (_, body) = req(
        &app,
        "GET",
        &format!("/admin/connectors/{id}"),
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(body["connector"]["name"], "after");

    // DELETE removes it.
    let (status, _) = req(
        &app,
        "DELETE",
        &format!("/admin/connectors/{id}"),
        Some(&token("a", "admin")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = req(
        &app,
        "GET",
        &format!("/admin/connectors/{id}"),
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A second delete 404s.
    let (status, _) = req(
        &app,
        "DELETE",
        &format!("/admin/connectors/{id}"),
        Some(&token("a", "admin")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_kind_is_rejected_with_400() {
    let app = app();
    let create = json!({ "name": "x", "kind": "slack", "config": {} });
    let (status, body) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("a", "admin")),
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "VALIDATION_ERROR");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("slack"),
        "message names the bad kind: {body:?}"
    );
}

#[tokio::test]
async fn malformed_config_is_rejected_with_400() {
    let app = app();
    // web kind requires a `url`.
    let create = json!({ "name": "x", "kind": "web", "config": { "not_url": "y" } });
    let (status, body) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("a", "admin")),
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "VALIDATION_ERROR");
}

// ---------------------------------------------------------------------------
// No-secret-leak
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_ref_is_stored_but_token_never_returned() {
    let app = app();
    // Create a github connector carrying an auth_ref (a NAME, not a token).
    let create = json!({
        "name": "private repo",
        "kind": "github",
        "config": { "owner": "smooai", "repo": "secret", "auth_ref": "GITHUB_TOKEN", "visibility": "private" },
        "enabled": true
    });
    let (status, body) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("a", "admin")),
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["connector"]["id"].as_str().unwrap().to_string();

    // The create response echoes the auth_ref NAME but never a token value.
    let create_text = serde_json::to_string(&body).unwrap();
    assert!(
        create_text.contains("GITHUB_TOKEN"),
        "auth_ref name is fine to echo"
    );
    assert!(
        !create_text.to_lowercase().contains("ghp_"),
        "no PAT in response"
    );

    // GET by id + list: never any token material, only the ref name.
    for path in [
        format!("/admin/connectors/{id}"),
        "/admin/connectors".to_string(),
    ] {
        let (_, body) = req(&app, "GET", &path, Some(&token("c", "curator")), None).await;
        let text = serde_json::to_string(&body).unwrap();
        assert!(
            !text.to_lowercase().contains("ghp_"),
            "no token leaked in {path}: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// Trigger an indexing run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_runs_offline_connector_and_appears_in_runs() {
    // A temp file the `file` connector ingests with no network.
    let dir = std::env::temp_dir().join(format!("smooth-index-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("note.md");
    std::fs::write(
        &file,
        "SmooAI offline indexing smoke document. Returns are accepted within 17 days.",
    )
    .unwrap();

    let app = app();
    let create = json!({
        "name": "local docs",
        "kind": "file",
        "config": { "path": file.to_string_lossy() },
        "enabled": true
    });
    let (status, body) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("a", "admin")),
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["connector"]["id"].as_str().unwrap().to_string();

    // Curator triggers an index run.
    let (status, body) = req(
        &app,
        "POST",
        &format!("/admin/connectors/{id}/index"),
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "index: {body:?}");
    assert_eq!(body["run"]["status"], "succeeded", "{body:?}");
    assert_eq!(body["run"]["connectorName"], "local docs");
    assert!(body["run"]["documentsSeen"].as_u64().unwrap() >= 1);
    assert!(body["run"]["chunksIndexed"].as_u64().unwrap() >= 1);

    // The run now appears in GET /admin/indexing/runs (reuses the IndexingStore).
    let (status, body) = req(
        &app,
        "GET",
        "/admin/indexing/runs",
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["runs"].as_array().expect("runs");
    assert!(
        runs.iter()
            .any(|r| r["connectorName"] == "local docs" && r["status"] == "succeeded"),
        "the triggered run is listed: {body:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn index_github_without_token_is_clean_400_no_network() {
    let app = app();
    // github connector with an auth_ref pointing at an UNSET env var.
    let unset = format!("SMOOTH_TEST_MISSING_{}", uuid::Uuid::new_v4().simple());
    let create = json!({
        "name": "needs token",
        "kind": "github",
        "config": { "owner": "smooai", "repo": "private", "auth_ref": unset, "visibility": "private" },
        "enabled": true
    });
    let (_, body) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("a", "admin")),
        Some(create),
    )
    .await;
    let id = body["connector"]["id"].as_str().unwrap().to_string();

    // Triggering index must 400 cleanly (token unresolvable) — no panic, no GitHub call.
    let (status, body) = req(
        &app,
        "POST",
        &format!("/admin/connectors/{id}/index"),
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "VALIDATION_ERROR");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("auth_ref") || msg.to_lowercase().contains("token"),
        "explains the missing secret: {msg}"
    );
}

#[tokio::test]
async fn index_requires_curator() {
    let app = app();
    let create = json!({ "name": "x", "kind": "web", "config": { "url": "https://e.test" } });
    let (_, body) = req(
        &app,
        "POST",
        "/admin/connectors",
        Some(&token("a", "admin")),
        Some(create),
    )
    .await;
    let id = body["connector"]["id"].as_str().unwrap().to_string();

    // Basic is forbidden from triggering an index.
    let (status, _) = req(
        &app,
        "POST",
        &format!("/admin/connectors/{id}/index"),
        Some(&token("u", "basic")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn index_unknown_connector_is_404() {
    let app = app();
    let (status, _) = req(
        &app,
        "POST",
        "/admin/connectors/missing/index",
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn settings_get_returns_defaults_then_put_reflects() {
    let app = app();
    // Default GET (Curator can read).
    let (status, body) = req(
        &app,
        "GET",
        "/admin/settings",
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["settings"]["orgId"], ORG);
    assert!(body["settings"]["model"].as_str().is_some());

    // Admin PUTs new settings.
    let update = json!({
        "model": "claude-test",
        "systemPrompt": "be terse",
        "defaultTools": ["knowledge_search"]
    });
    let (status, body) = req(
        &app,
        "PUT",
        "/admin/settings",
        Some(&token("a", "admin")),
        Some(update),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["settings"]["model"], "claude-test");

    // GET reflects the change.
    let (_, body) = req(
        &app,
        "GET",
        "/admin/settings",
        Some(&token("c", "curator")),
        None,
    )
    .await;
    assert_eq!(body["settings"]["model"], "claude-test");
    assert_eq!(body["settings"]["systemPrompt"], "be terse");
    assert_eq!(body["settings"]["defaultTools"][0], "knowledge_search");
}

#[tokio::test]
async fn settings_put_requires_admin() {
    let app = app();
    let update = json!({ "model": "x", "systemPrompt": "y", "defaultTools": [] });
    // Basic is forbidden.
    let (status, _) = req(
        &app,
        "PUT",
        "/admin/settings",
        Some(&token("u", "basic")),
        Some(update.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Curator is also forbidden (write is Admin-only).
    let (status, _) = req(
        &app,
        "PUT",
        "/admin/settings",
        Some(&token("c", "curator")),
        Some(update),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn settings_put_is_org_scoped() {
    let app = app();
    // Admin in ORG sets a model.
    let update = json!({ "model": "org-acme-model", "systemPrompt": "p", "defaultTools": [] });
    let (status, _) = req(
        &app,
        "PUT",
        "/admin/settings",
        Some(&token("a", "admin")),
        Some(update),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Admin in OTHER_ORG still sees defaults (not ORG's value).
    let (_, body) = req(
        &app,
        "GET",
        "/admin/settings",
        Some(&token_in(OTHER_ORG, "a2", "admin")),
        None,
    )
    .await;
    assert_ne!(body["settings"]["model"], "org-acme-model");
    assert_eq!(body["settings"]["orgId"], OTHER_ORG);
}
