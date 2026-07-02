//! Integration tests for the admin HTTP API (Phase 12, increment 1).
//!
//! Drives the real axum router in-process via `tower::ServiceExt::oneshot` — no
//! live gateway or network. Auth runs through the **real** [`JwtVerifier`] with
//! HS256 tokens signed in-test, so the route gates, org-scoping, and
//! "Basic-sees-own" filtering are exercised end to end.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt;

use smooth_operator::auth::JwtVerifier;
use smooth_operator::domain::{Conversation, Participant, ParticipantType, Platform};
use smooth_operator_ingestion::indexing::{
    InMemoryIndexingStore, IndexingRun, IndexingRunStatus, IndexingStore,
};
use smooth_operator_server::config::ServerConfig;
use smooth_operator_server::state::AppState;
use smooth_operator_server::{build_state, router};

const SECRET: &[u8] = b"admin-api-test-secret";
// The seeded knowledge / document sets are recorded under the reference
// server's seed org (`server::SEED_ORG_ID`); the admin tests authenticate as
// that org so the org-scoped admin reads (document sets, indexing runs) line up.
const ORG: &str = "reference-org";

/// A minimal config for an in-memory, seeded-KB server (no LLM key).
fn test_config(seed_kb: bool) -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.test/v1".into(),
        gateway_key: None,
        model: "m".into(),
        seed_kb,
        max_iterations: 4,
        max_tokens: 128,
        storage: smooth_operator_server::config::StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: Vec::new(),
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

/// Build app state with the real HS256 JwtVerifier + an indexing store.
fn state_with_auth(seed_kb: bool, indexing: Arc<dyn IndexingStore>) -> AppState {
    build_state(test_config(seed_kb))
        .with_auth(Arc::new(JwtVerifier::hs256(SECRET, None, None)))
        .with_indexing(indexing)
}

/// Sign an HS256 token for `(user, role)` in [`ORG`].
fn token(user: &str, role: &str) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
    let claims = json!({
        "sub": user,
        "org": ORG,
        "role": role,
        "name": format!("{user} display"),
        "exp": exp,
    });
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(SECRET),
    )
    .expect("sign")
}

/// GET `path` with an optional bearer token; return `(status, json body)`.
async fn get(app: &axum::Router, path: &str, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().method("GET").uri(path);
    if let Some(t) = bearer {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

/// Seed a conversation in `ORG` with a single `User` participant whose
/// `external_id` is `owner_user`. Returns the conversation id.
async fn seed_conversation(state: &AppState, owner_user: &str, name: &str) -> String {
    let now = chrono::Utc::now();
    let conv_id = uuid::Uuid::new_v4().to_string();
    let conv = Conversation {
        id: conv_id.clone(),
        platform: Platform::Web,
        name: name.to_string(),
        organization_id: ORG.to_string(),
        idempotency_key: conv_id.clone(),
        metadata_json: None,
        analytics_json: None,
        created_at: now,
        updated_at: now,
    };
    state
        .storage
        .create_conversation(conv)
        .await
        .expect("create conv");

    let participant = Participant {
        id: uuid::Uuid::new_v4().to_string(),
        conversation_id: conv_id.clone(),
        organization_id: ORG.to_string(),
        participant_type: ParticipantType::User,
        external_id: Some(owner_user.to_string()),
        internal_id: None,
        browser_fingerprint: None,
        browser_info: None,
        name: owner_user.to_string(),
        email: None,
        phone: None,
        crm_contact_id: None,
        metadata_json: None,
        created_at: now,
        updated_at: now,
    };
    state
        .storage
        .add_participant(participant)
        .await
        .expect("add participant");
    conv_id
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_is_unauthenticated() {
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    let app = router(state);
    let (status, body) = get(&app, "/admin/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn admin_response_carries_cors_header() {
    // A real cross-origin GET /admin/me must carry `access-control-allow-origin`
    // so the daemon's smooth-web SPA (running on the Vite dev origin) can read the
    // model/identity. Auth is unchanged — the request still needs a valid token.
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/me")
                .header("Origin", "http://localhost:3100")
                .header(
                    "Authorization",
                    format!("Bearer {}", token("alice", "admin")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap()),
        Some("*"),
        "the /admin response must carry a permissive CORS allow-origin header"
    );
}

#[tokio::test]
async fn admin_cors_preflight_allows_authorization_header() {
    // The browser preflights GET /admin/me (it carries an Authorization header)
    // with an OPTIONS request; the response must allow the `authorization` request
    // header, else the real request is blocked.
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/admin/me")
                .header("Origin", "http://localhost:3100")
                .header("Access-Control-Request-Method", "GET")
                .header("Access-Control-Request-Headers", "authorization")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    // tower-http answers the preflight directly (200/204) with the allow headers.
    assert!(
        resp.status().is_success(),
        "CORS preflight should succeed, got {}",
        resp.status()
    );
    let allow_headers = resp
        .headers()
        .get("access-control-allow-headers")
        .map(|v| v.to_str().unwrap().to_ascii_lowercase())
        .unwrap_or_default();
    assert!(
        allow_headers.contains("authorization"),
        "preflight must allow the authorization header, got: {allow_headers:?}"
    );
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap()),
        Some("*"),
        "preflight must echo a permissive allow-origin"
    );
}

#[tokio::test]
async fn me_returns_principal() {
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    let app = router(state);
    let (status, body) = get(&app, "/admin/me", Some(&token("alice", "admin"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["userId"], "alice");
    assert_eq!(body["orgId"], ORG);
    assert_eq!(body["role"], "admin");
    assert_eq!(body["displayName"], "alice display");
}

#[tokio::test]
async fn me_without_token_is_401() {
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    let app = router(state);
    let (status, body) = get(&app, "/admin/me", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "UNAUTHENTICATED");
}

#[tokio::test]
async fn me_with_garbage_token_is_401() {
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    let app = router(state);
    let (status, body) = get(&app, "/admin/me", Some("not-a-jwt")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "INVALID_TOKEN");
}

#[tokio::test]
async fn admin_sees_all_org_conversations() {
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    seed_conversation(&state, "alice", "Alice convo").await;
    seed_conversation(&state, "bob", "Bob convo").await;
    let app = router(state);

    let (status, body) = get(
        &app,
        "/admin/conversations",
        Some(&token("admin1", "admin")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let convs = body["conversations"].as_array().expect("array");
    assert_eq!(convs.len(), 2, "admin sees the whole org: {body:?}");
}

#[tokio::test]
async fn curator_sees_all_org_conversations() {
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    seed_conversation(&state, "alice", "Alice convo").await;
    seed_conversation(&state, "bob", "Bob convo").await;
    let app = router(state);

    let (status, body) = get(&app, "/admin/conversations", Some(&token("cur", "curator"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["conversations"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn basic_sees_only_own_conversations() {
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    seed_conversation(&state, "alice", "Alice convo").await;
    seed_conversation(&state, "bob", "Bob convo").await;
    let app = router(state);

    // Alice (basic) sees only her own conversation.
    let (status, body) = get(&app, "/admin/conversations", Some(&token("alice", "basic"))).await;
    assert_eq!(status, StatusCode::OK);
    let convs = body["conversations"].as_array().expect("array");
    assert_eq!(convs.len(), 1, "basic sees only own: {body:?}");
    assert_eq!(convs[0]["name"], "Alice convo");
}

#[tokio::test]
async fn basic_can_read_own_messages_but_not_others() {
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    let alice_conv = seed_conversation(&state, "alice", "Alice convo").await;
    let bob_conv = seed_conversation(&state, "bob", "Bob convo").await;
    let app = router(state);

    // Own conversation → 200.
    let (status, body) = get(
        &app,
        &format!("/admin/conversations/{alice_conv}/messages"),
        Some(&token("alice", "basic")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["conversationId"], alice_conv);
    assert!(body["messages"].is_array());

    // Someone else's conversation → 403.
    let (status, body) = get(
        &app,
        &format!("/admin/conversations/{bob_conv}/messages"),
        Some(&token("alice", "basic")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "FORBIDDEN");

    // Admin can read anyone's.
    let (status, _) = get(
        &app,
        &format!("/admin/conversations/{bob_conv}/messages"),
        Some(&token("admin1", "admin")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn messages_for_unknown_conversation_is_404() {
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    let app = router(state);
    let (status, body) = get(
        &app,
        "/admin/conversations/does-not-exist/messages",
        Some(&token("admin1", "admin")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn indexing_runs_requires_curator() {
    let store = Arc::new(InMemoryIndexingStore::new());
    // Record one succeeded run for connector "github" under the ORG-NAMESPACED
    // key the admin API now uses (cross-org scoping): runs are keyed by
    // `scoped_connector_key(org, name)`, not the bare connector name.
    let now = chrono::Utc::now();
    store.record_run(&IndexingRun {
        id: "run-1".into(),
        connector_name: smooth_operator_server::state::scoped_connector_key(ORG, "github"),
        status: IndexingRunStatus::Succeeded,
        started_at: now,
        finished_at: Some(now),
        documents_seen: 3,
        chunks_indexed: 9,
        documents_skipped: 1,
        cursor: Some(now),
        error: None,
    });
    let state = state_with_auth(false, store);
    state.record_connector(ORG, "github");
    let app = router(state);

    // Basic is forbidden.
    let (status, body) = get(&app, "/admin/indexing/runs", Some(&token("u", "basic"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "FORBIDDEN");

    // Curator gets the run.
    let (status, body) = get(&app, "/admin/indexing/runs", Some(&token("u", "curator"))).await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["connectorName"], "github");
    assert_eq!(runs[0]["status"], "succeeded");
    assert_eq!(runs[0]["documentsSeen"], 3);
    assert_eq!(runs[0]["chunksIndexed"], 9);
}

#[tokio::test]
async fn document_sets_lists_seeded_set() {
    // Seeded KB tags both demo docs into the "policies" set.
    let state = state_with_auth(true, Arc::new(InMemoryIndexingStore::new()));
    let app = router(state);

    // Basic is forbidden.
    let (status, _) = get(&app, "/admin/document-sets", Some(&token("u", "basic"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Curator sees the set with its doc count.
    let (status, body) = get(&app, "/admin/document-sets", Some(&token("u", "curator"))).await;
    assert_eq!(status, StatusCode::OK);
    let sets = body["documentSets"].as_array().expect("sets array");
    assert_eq!(sets.len(), 1, "one seeded set: {body:?}");
    assert_eq!(sets[0]["name"], "policies");
    assert_eq!(sets[0]["documentCount"], 2);
}

#[tokio::test]
async fn ws_route_still_works() {
    // The admin router merge must not break the existing /ws upgrade route.
    // A plain GET without upgrade headers should be rejected by the ws handler
    // (not 404'd), proving the route is still mounted.
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ws")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    // axum's WS upgrade rejects a non-upgrade GET with 400/426 — crucially not
    // 404, which would mean the route vanished.
    assert_ne!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "/ws route must remain"
    );
}

// ---------------------------------------------------------------------------
// Cross-org isolation (cross-org leak fix)
// ---------------------------------------------------------------------------

/// Sign a curator token for a SPECIFIC org (overrides the default [`ORG`]).
fn token_for_org(user: &str, role: &str, org: &str) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
    let claims = json!({
        "sub": user, "org": org, "role": role,
        "name": format!("{user} display"), "exp": exp,
    });
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(SECRET),
    )
    .expect("sign")
}

/// `GET /admin/indexing/runs`: org A's runs are NOT visible to an org-B caller,
/// and two orgs with a **same-named** connector ("docs") see only their own
/// runs (no key collision). This is the cross-org leak the fix closes — the
/// indexing store was keyed by bare connector name, so org B saw org A's runs.
#[tokio::test]
async fn indexing_runs_are_org_scoped_and_same_name_connectors_dont_collide() {
    use smooth_operator_server::state::scoped_connector_key;

    let store = Arc::new(InMemoryIndexingStore::new());
    let now = chrono::Utc::now();
    let mk_run = |id: &str, key: String, seen: usize| IndexingRun {
        id: id.into(),
        connector_name: key,
        status: IndexingRunStatus::Succeeded,
        started_at: now,
        finished_at: Some(now),
        documents_seen: seen,
        chunks_indexed: seen,
        documents_skipped: 0,
        cursor: Some(now),
        error: None,
    };
    // Org A and Org B each have a connector NAMED "docs" — distinct runs.
    store.record_run(&mk_run("run-a", scoped_connector_key("org-a", "docs"), 11));
    store.record_run(&mk_run("run-b", scoped_connector_key("org-b", "docs"), 22));

    let state = state_with_auth(false, store);
    state.record_connector("org-a", "docs");
    state.record_connector("org-b", "docs");
    let app = router(state);

    // Org A's curator sees ONLY org A's run (documentsSeen=11), never org B's.
    let (status, body) = get(
        &app,
        "/admin/indexing/runs",
        Some(&token_for_org("ua", "curator", "org-a")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["runs"].as_array().expect("runs");
    assert_eq!(
        runs.len(),
        1,
        "org A must see exactly its own run: {body:?}"
    );
    assert_eq!(runs[0]["connectorName"], "docs");
    assert_eq!(
        runs[0]["documentsSeen"], 11,
        "LEAK: org A saw a run that wasn't its own"
    );

    // Org B's curator sees ONLY org B's run (documentsSeen=22).
    let (status, body) = get(
        &app,
        "/admin/indexing/runs",
        Some(&token_for_org("ub", "curator", "org-b")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 1, "org B must see exactly its own run");
    assert_eq!(
        runs[0]["documentsSeen"], 22,
        "LEAK: org B saw a run that wasn't its own"
    );

    // A THIRD org with nothing recorded sees nothing.
    let (status, body) = get(
        &app,
        "/admin/indexing/runs",
        Some(&token_for_org("uc", "curator", "org-c")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["runs"].as_array().expect("runs").is_empty(),
        "an org with no connectors must see no runs (no cross-org leak)"
    );
}

/// `GET /admin/document-sets`: org A's document sets are NOT visible to an
/// org-B caller. The doc-set registry was global (cross-org leak); it is now
/// org-keyed.
#[tokio::test]
async fn document_sets_are_org_scoped() {
    // Unseeded server so the only sets are the ones we record per org.
    let state = state_with_auth(false, Arc::new(InMemoryIndexingStore::new()));
    state.record_document_set("org-a", "handbook");
    state.record_document_set("org-a", "handbook");
    state.record_document_set("org-b", "secrets");
    let app = router(state);

    // Org A sees only "handbook" (count 2), never org B's "secrets".
    let (status, body) = get(
        &app,
        "/admin/document-sets",
        Some(&token_for_org("ua", "curator", "org-a")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sets = body["documentSets"].as_array().expect("sets");
    assert_eq!(sets.len(), 1, "org A sees exactly its own set: {body:?}");
    assert_eq!(sets[0]["name"], "handbook");
    assert_eq!(sets[0]["documentCount"], 2);
    assert!(
        !sets.iter().any(|s| s["name"] == "secrets"),
        "LEAK: org A saw org B's document set"
    );

    // Org B sees only "secrets".
    let (status, body) = get(
        &app,
        "/admin/document-sets",
        Some(&token_for_org("ub", "curator", "org-b")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sets = body["documentSets"].as_array().expect("sets");
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0]["name"], "secrets");
    assert!(
        !sets.iter().any(|s| s["name"] == "handbook"),
        "LEAK: org B saw org A's document set"
    );
}
