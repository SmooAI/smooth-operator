//! The admin HTTP API (Phase 12, increment 1).
//!
//! A REST surface, mounted under `/admin`, that the Next.js management console
//! (increment 2) consumes: whoami, chat history, indexing status, and document
//! sets. Everything except `/admin/health` is gated by [`require_role`] and
//! org-scoped to the caller's [`Principal`].
//!
//! ## Routes + role gates
//!
//! | route | min role | scope |
//! | --- | --- | --- |
//! | `GET /admin/health` | — (public) | liveness only |
//! | `GET /admin/me` | Basic | the caller's own principal |
//! | `GET /admin/conversations` | Basic | Admin/Curator: org-wide; Basic: own only |
//! | `GET /admin/conversations/{id}/messages` | Basic | role-scoped (Basic must own the convo) |
//! | `GET /admin/indexing/runs` | Curator | org connectors |
//! | `GET /admin/document-sets` | Curator | org document sets |
//!
//! ## Org-scoping + "Basic sees own"
//!
//! Every read filters to `principal.org_id` (the storage adapter's
//! `list_conversations_by_org`). For a **Basic** caller, the result is further
//! narrowed to conversations the caller *owns*: a conversation is owned when one
//! of its `User` participants carries `external_id == principal.user_id`. An
//! Admin or Curator sees the whole org. This mirrors the document-level
//! [`AccessContext`](smooth_operator::access_control::AccessContext) model RBAC
//! sits on top of.
//!
//! ## Errors
//!
//! Auth failures map to clean status codes (401 unauthenticated / invalid token /
//! missing role; 403 insufficient role) with the protocol's `error` envelope
//! shape (`{ code, message }`) reused for the body. Never leaks a token.

use axum::extract::{Path, Query, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use smooth_operator::auth::{AuthError, Principal, Role};
use smooth_operator::backplane::Target;
use smooth_operator::connector_config::{ConnectorConfig, ConnectorKind};
use smooth_operator::domain::ParticipantType;
use smooth_operator::settings::AgentSettings;

use smooth_operator_ingestion::{
    Chunker, Connector, FileConnector, GithubAuth, GithubConnector, GithubConnectorConfig,
    GithubVisibility, IndexingService, WebConnector,
};

use crate::embedder::{build_embedder, EmbedderConfig};
use crate::protocol;
use crate::state::{scoped_connector_key, AppState};

/// Build the `/admin` router over the shared [`AppState`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/health", get(health))
        .route("/admin/me", get(me))
        .route("/admin/conversations", get(list_conversations))
        .route(
            "/admin/conversations/{id}/messages",
            get(conversation_messages),
        )
        .route("/admin/indexing/runs", get(indexing_runs))
        .route("/admin/document-sets", get(document_sets))
        // Write API (Phase 12, increment 3) — connector CRUD, index trigger,
        // settings. RBAC: list/get are Curator; create/update/delete are Admin;
        // index trigger is Curator; settings read is Curator, write is Admin.
        .route(
            "/admin/connectors",
            get(list_connectors).post(create_connector),
        )
        .route(
            "/admin/connectors/{id}",
            get(get_connector)
                .put(update_connector)
                .delete(delete_connector),
        )
        .route("/admin/connectors/{id}/index", post(index_connector))
        .route("/admin/settings", get(get_settings).put(put_settings))
        // Realtime publish (Phase: backplane) — push an event to a backplane
        // target over the WebSocket fleet. The plug point for non-AI publishers
        // (job status, ingestion progress, notifications). Admin-gated.
        .route("/admin/publish", post(publish_event))
}

// ---------------------------------------------------------------------------
// Auth extractor — `require_role`
// ---------------------------------------------------------------------------

/// An authenticated [`Principal`] guaranteed to hold at least role `MIN`.
///
/// Used as an axum extractor: it reads `Authorization: Bearer <token>`, verifies
/// it via the configured [`AuthVerifier`](smooth_operator::auth::AuthVerifier) in
/// [`AppState`], and rejects with 401/403 if the token is missing/invalid or the
/// role is insufficient — *before* the handler body runs. `MIN` is a const role
/// rank: `0 = Basic`, `1 = Curator`, `2 = Admin`.
pub struct RequireRole<const MIN: u8>(pub Principal);

/// Map a [`Role`] to the const rank used by [`RequireRole`].
const fn role_rank(role: Role) -> u8 {
    match role {
        Role::Basic => 0,
        Role::Curator => 1,
        Role::Admin => 2,
    }
}

/// The minimum [`Role`] a const rank denotes (for error messages).
const fn rank_role(min: u8) -> Role {
    match min {
        0 => Role::Basic,
        1 => Role::Curator,
        _ => Role::Admin,
    }
}

impl<const MIN: u8> axum::extract::FromRequestParts<AppState> for RequireRole<MIN> {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(parts).ok_or(AuthRejection(AuthError::Unauthenticated))?;
        let principal = state.auth.verify(&token).map_err(AuthRejection)?;
        if role_rank(principal.role) < MIN {
            return Err(AuthRejection(AuthError::Forbidden {
                required: rank_role(MIN),
                actual: principal.role,
            }));
        }
        Ok(RequireRole(principal))
    }
}

/// Extract the raw bearer token (without the `Bearer ` prefix) from the
/// `Authorization` header. Returns `None` when absent or not a bearer scheme.
fn bearer_token(parts: &Parts) -> Option<String> {
    let header = parts.headers.get(axum::http::header::AUTHORIZATION)?;
    let value = header.to_str().ok()?;
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// An auth/authorization rejection rendered as the protocol's `error` envelope
/// with the right HTTP status.
pub struct AuthRejection(AuthError);

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        let (status, code) = match &self.0 {
            AuthError::Unauthenticated => (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED"),
            AuthError::InvalidToken(_) => (StatusCode::UNAUTHORIZED, "INVALID_TOKEN"),
            AuthError::MissingRole(_) => (StatusCode::UNAUTHORIZED, "MISSING_ROLE"),
            AuthError::Forbidden { .. } => (StatusCode::FORBIDDEN, "FORBIDDEN"),
            // A misconfigured verifier is a server error, surfaced as 500 with a
            // non-leaking message.
            AuthError::Misconfigured(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "AUTH_MISCONFIGURED")
            }
        };
        let body = protocol::error(None, code, &self.0.to_string());
        (status, Json(body)).into_response()
    }
}

/// An error from a handler body (storage failure, etc.) rendered as a 500 with
/// the protocol error shape.
struct AdminError(StatusCode, String, &'static str);

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let body = protocol::error(None, self.2, &self.1);
        (self.0, Json(body)).into_response()
    }
}

impl AdminError {
    fn internal(msg: impl Into<String>) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            msg.into(),
            "INTERNAL_ERROR",
        )
    }

    fn forbidden(msg: impl Into<String>) -> Self {
        Self(StatusCode::FORBIDDEN, msg.into(), "FORBIDDEN")
    }

    fn not_found(msg: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, msg.into(), "NOT_FOUND")
    }

    /// A 400 with the protocol's `VALIDATION_ERROR` code — used for unknown
    /// connector kinds, malformed config payloads, and unresolvable `auth_ref`s.
    fn validation(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into(), "VALIDATION_ERROR")
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /admin/health` — unauthenticated liveness probe.
async fn health() -> Json<Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

/// `GET /admin/me` — whoami. Returns the authenticated principal (any role).
async fn me(RequireRole::<0>(principal): RequireRole<0>) -> Json<Principal> {
    Json(principal)
}

/// Query params for `GET /admin/conversations`.
#[derive(Debug, Deserialize)]
struct ConversationsQuery {
    /// Max conversations to return (defaults to 50, capped at 200).
    limit: Option<usize>,
    /// Opaque cursor: the index to start from (simple offset paging over the
    /// org-scoped, newest-first list). `None` ⇒ start at the beginning.
    cursor: Option<usize>,
}

/// A conversation row in the admin list response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversationRow {
    id: String,
    name: String,
    platform: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

/// The `GET /admin/conversations` response envelope.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversationsResponse {
    conversations: Vec<ConversationRow>,
    /// Opaque cursor for the next page, or `null` when exhausted.
    next_cursor: Option<usize>,
}

/// `GET /admin/conversations` — chat history, org-scoped. Admin/Curator see the
/// whole org; Basic sees only conversations they own.
async fn list_conversations(
    RequireRole::<0>(principal): RequireRole<0>,
    State(state): State<AppState>,
    Query(q): Query<ConversationsQuery>,
) -> Result<Json<ConversationsResponse>, AdminError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.cursor.unwrap_or(0);

    let all = state
        .storage
        .list_conversations_by_org(&principal.org_id)
        .await
        .map_err(|e| AdminError::internal(format!("list conversations failed: {e}")))?;

    // Basic callers only see conversations they own.
    let visible: Vec<_> = if principal.role >= Role::Curator {
        all
    } else {
        let mut owned = Vec::new();
        for conv in all {
            if conversation_owned_by(&state, &conv.id, &principal.user_id).await {
                owned.push(conv);
            }
        }
        owned
    };

    let total = visible.len();
    let page: Vec<ConversationRow> = visible
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|c| ConversationRow {
            id: c.id,
            name: c.name,
            platform: format!("{:?}", c.platform).to_lowercase(),
            created_at: c.created_at,
            updated_at: c.updated_at,
        })
        .collect();

    let next = offset + page.len();
    let next_cursor = if next < total { Some(next) } else { None };

    Ok(Json(ConversationsResponse {
        conversations: page,
        next_cursor,
    }))
}

/// `GET /admin/conversations/{id}/messages` — messages for one conversation,
/// role-scoped (a Basic caller must own the conversation).
async fn conversation_messages(
    RequireRole::<0>(principal): RequireRole<0>,
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
) -> Result<Json<Value>, AdminError> {
    // The conversation must exist + belong to the caller's org.
    let conv = state
        .storage
        .get_conversation(&conversation_id)
        .await
        .map_err(|e| AdminError::internal(format!("get conversation failed: {e}")))?
        .ok_or_else(|| {
            AdminError::not_found(format!("conversation '{conversation_id}' not found"))
        })?;

    if conv.organization_id != principal.org_id {
        // Don't leak existence across orgs — 404, not 403.
        return Err(AdminError::not_found(format!(
            "conversation '{conversation_id}' not found"
        )));
    }

    // Basic callers may only read conversations they own.
    if principal.role < Role::Curator
        && !conversation_owned_by(&state, &conversation_id, &principal.user_id).await
    {
        return Err(AdminError::forbidden(
            "you do not have access to this conversation",
        ));
    }

    let query = smooth_operator::adapter::MessageQuery::new(&conversation_id, 200);
    let page = state
        .storage
        .list_messages_by_conversation(query)
        .await
        .map_err(|e| AdminError::internal(format!("list messages failed: {e}")))?;

    Ok(Json(serde_json::json!({
        "conversationId": conversation_id,
        "messages": page.messages,
        "nextCursor": page.next_cursor,
    })))
}

/// `GET /admin/indexing/runs` — indexing-run status across **the caller's org's**
/// connectors. Curator+ only.
///
/// Org-scoped two ways (cross-org leak fix): we iterate only the principal's
/// org's connectors, and we query the indexing store under the **org-namespaced**
/// key ([`scoped_connector_key`]) so a same-named connector in another org can't
/// surface its runs here. The reported `connectorName` is the un-scoped display
/// name (the namespace is an internal storage key, never exposed).
async fn indexing_runs(
    RequireRole::<1>(principal): RequireRole<1>,
    State(state): State<AppState>,
) -> Json<Value> {
    let mut runs = Vec::new();
    for connector in state.connectors(&principal.org_id) {
        let key = scoped_connector_key(&principal.org_id, &connector);
        for run in state.indexing.list_runs(&key) {
            runs.push(serde_json::json!({
                "id": run.id,
                // Report the display name, never the internal org-namespaced key.
                "connectorName": connector,
                "status": format!("{:?}", run.status).to_lowercase(),
                "startedAt": run.started_at,
                "finishedAt": run.finished_at,
                "documentsSeen": run.documents_seen,
                "chunksIndexed": run.chunks_indexed,
                "documentsSkipped": run.documents_skipped,
                "cursor": run.cursor,
                "error": run.error,
            }));
        }
    }
    Json(serde_json::json!({ "runs": runs }))
}

/// A document-set row.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentSetRow {
    name: String,
    document_count: usize,
}

/// `GET /admin/document-sets` — distinct document-set names + doc counts for
/// **the caller's org**. Curator+ only. Org-scoped so org A's document sets are
/// never reported to an org-B caller (cross-org leak fix).
async fn document_sets(
    RequireRole::<1>(principal): RequireRole<1>,
    State(state): State<AppState>,
) -> Json<Value> {
    let sets: Vec<DocumentSetRow> = state
        .document_sets(&principal.org_id)
        .into_iter()
        .map(|(name, document_count)| DocumentSetRow {
            name,
            document_count,
        })
        .collect();
    Json(serde_json::json!({ "documentSets": sets }))
}

// ---------------------------------------------------------------------------
// Connector config CRUD (Phase 12, increment 3)
// ---------------------------------------------------------------------------

/// The wire body for create/update of a connector. `kind` is validated against
/// [`ConnectorKind`]; `config` is the kind-specific free-form payload (may carry
/// an `auth_ref` naming a secret — never the secret itself).
#[derive(Debug, Deserialize)]
struct ConnectorWrite {
    name: String,
    kind: String,
    #[serde(default)]
    config: Value,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

const fn default_enabled() -> bool {
    true
}

/// Serialize a [`ConnectorConfig`] for an API response under a `connector` key.
///
/// The stored `config` is echoed as-is — it only ever holds an `auth_ref` *name*,
/// never a secret value, so this can never leak a credential.
fn connector_json(cfg: &ConnectorConfig) -> Value {
    serde_json::json!({
        "connector": {
            "id": cfg.id,
            "name": cfg.name,
            "kind": cfg.kind.as_str(),
            "config": cfg.config,
            "enabled": cfg.enabled,
            "createdAt": cfg.created_at,
            "updatedAt": cfg.updated_at,
        }
    })
}

/// Validate `(kind, config)` and surface a clean 400 on an unknown kind or a
/// payload missing the fields that kind needs to build a connector.
fn validate_connector(kind: ConnectorKind, config: &Value) -> Result<(), AdminError> {
    let missing = |field: &str| {
        AdminError::validation(format!(
            "{} connector config requires a '{field}' field",
            kind.as_str()
        ))
    };
    match kind {
        ConnectorKind::Github => {
            if config.get("owner").and_then(Value::as_str).is_none() {
                return Err(missing("owner"));
            }
            if config.get("repo").and_then(Value::as_str).is_none() {
                return Err(missing("repo"));
            }
        }
        ConnectorKind::Web => {
            if config.get("url").and_then(Value::as_str).is_none() {
                return Err(missing("url"));
            }
        }
        ConnectorKind::File => {
            if config.get("path").and_then(Value::as_str).is_none() {
                return Err(missing("path"));
            }
        }
    }
    Ok(())
}

/// `GET /admin/connectors` — list this org's connectors (Curator+).
async fn list_connectors(
    RequireRole::<1>(principal): RequireRole<1>,
    State(state): State<AppState>,
) -> Json<Value> {
    let connectors: Vec<Value> = state
        .connector_configs
        .list(&principal.org_id)
        .iter()
        .map(|c| connector_json(c)["connector"].clone())
        .collect();
    Json(serde_json::json!({ "connectors": connectors }))
}

/// `GET /admin/connectors/{id}` — one connector, org-scoped (Curator+). A
/// cross-org / unknown id is a 404 (existence not leaked across orgs).
async fn get_connector(
    RequireRole::<1>(principal): RequireRole<1>,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AdminError> {
    let cfg = state
        .connector_configs
        .get(&principal.org_id, &id)
        .ok_or_else(|| AdminError::not_found(format!("connector '{id}' not found")))?;
    Ok(Json(connector_json(&cfg)))
}

/// `POST /admin/connectors` — create a connector (Admin only). Returns 201 with
/// the created connector (a fresh uuid id, org from the principal).
async fn create_connector(
    RequireRole::<2>(principal): RequireRole<2>,
    State(state): State<AppState>,
    Json(body): Json<ConnectorWrite>,
) -> Result<Response, AdminError> {
    let kind = ConnectorKind::parse(&body.kind)
        .map_err(|bad| AdminError::validation(format!("unknown connector kind '{bad}'")))?;
    validate_connector(kind, &body.config)?;

    let now = chrono::Utc::now();
    let cfg = ConnectorConfig {
        id: uuid::Uuid::new_v4().to_string(),
        org_id: principal.org_id.clone(),
        name: body.name,
        kind,
        config: body.config,
        enabled: body.enabled,
        created_at: now,
        updated_at: now,
    };
    state.connector_configs.upsert(cfg.clone());
    // Record the connector name under the caller's org so its runs are listed by
    // /admin/indexing/runs — and ONLY for this org (cross-org scoping).
    state.record_connector(principal.org_id.clone(), cfg.name.clone());
    Ok((StatusCode::CREATED, Json(connector_json(&cfg))).into_response())
}

/// `PUT /admin/connectors/{id}` — update a connector (Admin only). The id +
/// `created_at` are preserved; a cross-org / unknown id is a 404.
async fn update_connector(
    RequireRole::<2>(principal): RequireRole<2>,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ConnectorWrite>,
) -> Result<Json<Value>, AdminError> {
    let existing = state
        .connector_configs
        .get(&principal.org_id, &id)
        .ok_or_else(|| AdminError::not_found(format!("connector '{id}' not found")))?;

    let kind = ConnectorKind::parse(&body.kind)
        .map_err(|bad| AdminError::validation(format!("unknown connector kind '{bad}'")))?;
    validate_connector(kind, &body.config)?;

    let cfg = ConnectorConfig {
        id: existing.id,
        org_id: existing.org_id,
        name: body.name,
        kind,
        config: body.config,
        enabled: body.enabled,
        created_at: existing.created_at,
        updated_at: chrono::Utc::now(),
    };
    state.connector_configs.upsert(cfg.clone());
    state.record_connector(principal.org_id.clone(), cfg.name.clone());
    Ok(Json(connector_json(&cfg)))
}

/// `DELETE /admin/connectors/{id}` — remove a connector (Admin only). 204 on
/// success; a cross-org / unknown id is a 404.
async fn delete_connector(
    RequireRole::<2>(principal): RequireRole<2>,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    if state.connector_configs.delete(&principal.org_id, &id) {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(AdminError::not_found(format!("connector '{id}' not found")))
    }
}

/// `POST /admin/connectors/{id}/index` — build the connector from its stored
/// config and run one indexing pass (Curator+).
///
/// For `github`, the token is resolved from `auth_ref` → env at *this* moment
/// (never persisted). An unresolvable `auth_ref` returns a clean 400 *before*
/// any GitHub call — no panic, no network. The resulting [`IndexingRun`] is
/// recorded in the shared `IndexingStore` (so it also shows in
/// `GET /admin/indexing/runs`) and returned.
async fn index_connector(
    RequireRole::<1>(principal): RequireRole<1>,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AdminError> {
    let cfg = state
        .connector_configs
        .get(&principal.org_id, &id)
        .ok_or_else(|| AdminError::not_found(format!("connector '{id}' not found")))?;

    // Build the live connector from the stored config (resolving any secret from
    // env at this moment). A validation failure here is a clean 400.
    //
    // The connector is named with an ORG-NAMESPACED key so the indexing run is
    // recorded in the store under `IXCONN#<org>...<name>` — a same-named
    // connector in another org records + lists separately (cross-org scoping).
    // The display name (`cfg.name`) is rewritten back into the response below.
    let scoped_key = scoped_connector_key(&principal.org_id, &cfg.name);
    let connector = build_connector(&cfg, &scoped_key)?;

    let service = IndexingService::new(principal.org_id.clone());
    let chunker = Chunker::default();
    // Select the embedder from config: the real semantic GatewayEmbedder (1536-d)
    // when the gateway is keyed, else the network-free DeterministicEmbedder
    // (1024-d) with a loud warning. The knowledge store the docs land in was
    // created with this same embedder's dim by the storage-backend wiring
    // (`build_state_from_env_async`), so document and query vectors agree.
    let embedder = build_embedder(&EmbedderConfig::from_server_config(&state.config));
    let knowledge = state.storage.knowledge();

    let run = service
        .run_once(
            connector.as_ref(),
            state.indexing.as_ref(),
            &chunker,
            embedder.as_ref(),
            knowledge,
        )
        .await
        .map_err(|e| AdminError::internal(format!("indexing failed: {e}")))?;

    // Surface the connector for /admin/indexing/runs listing (org-scoped).
    state.record_connector(principal.org_id.clone(), cfg.name.clone());

    Ok(Json(serde_json::json!({
        "run": {
            "id": run.id,
            // Report the display name, never the internal org-namespaced key.
            "connectorName": cfg.name,
            "status": format!("{:?}", run.status).to_lowercase(),
            "startedAt": run.started_at,
            "finishedAt": run.finished_at,
            "documentsSeen": run.documents_seen,
            "chunksIndexed": run.chunks_indexed,
            "documentsSkipped": run.documents_skipped,
            "cursor": run.cursor,
            "error": run.error,
        }
    })))
}

/// Build a live [`Connector`] from a stored [`ConnectorConfig`], resolving any
/// secret named by `auth_ref` from the environment *now* (never persisted).
///
/// `connector_name` is the name stamped onto the built connector — the caller
/// passes an **org-namespaced** key so the indexing run is recorded per-org
/// (cross-org scoping), keeping the human display name out of the storage key.
///
/// Returns a clean 400 ([`AdminError::validation`]) — never a panic — for a
/// malformed config or an unresolvable `auth_ref`, so `/index` can surface the
/// problem without touching the network.
fn build_connector(
    cfg: &ConnectorConfig,
    connector_name: &str,
) -> Result<Box<dyn Connector>, AdminError> {
    let connector_name = connector_name.to_string();
    match cfg.kind {
        ConnectorKind::Web => {
            let url = cfg
                .config
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| AdminError::validation("web connector requires a 'url'"))?;
            Ok(Box::new(NamedConnector::new(
                connector_name,
                WebConnector::new(url),
            )))
        }
        ConnectorKind::File => {
            let path = cfg
                .config
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| AdminError::validation("file connector requires a 'path'"))?;
            Ok(Box::new(NamedConnector::new(
                connector_name,
                FileConnector::new(path),
            )))
        }
        ConnectorKind::Github => {
            let owner = cfg
                .config
                .get("owner")
                .and_then(Value::as_str)
                .ok_or_else(|| AdminError::validation("github connector requires an 'owner'"))?;
            let repo = cfg
                .config
                .get("repo")
                .and_then(Value::as_str)
                .ok_or_else(|| AdminError::validation("github connector requires a 'repo'"))?;

            // Resolve the token from auth_ref→env. A private repo MUST have a
            // resolvable token; a public repo may index unauthenticated.
            let visibility = match cfg.config.get("visibility").and_then(Value::as_str) {
                Some("private") => GithubVisibility::Private,
                _ => GithubVisibility::Public,
            };
            let auth = resolve_github_auth(cfg, visibility)?;

            let mut gh = GithubConnectorConfig::new(owner, repo, auth).visibility(visibility);
            if let Some(r) = cfg.config.get("ref").and_then(Value::as_str) {
                gh = gh.at_ref(r);
            }
            Ok(Box::new(NamedConnector::new(
                connector_name,
                GithubConnector::new(gh),
            )))
        }
    }
}

/// Resolve a [`GithubAuth`] from the connector's `auth_ref` → env var.
///
/// - `auth_ref` set + env present ⇒ `Token`.
/// - `auth_ref` set but env **missing/empty** ⇒ a clean 400 (no GitHub call).
/// - no `auth_ref`: a **public** repo indexes `Unauthenticated`; a **private**
///   repo is a 400 (a private repo needs a credential).
fn resolve_github_auth(
    cfg: &ConnectorConfig,
    visibility: GithubVisibility,
) -> Result<GithubAuth, AdminError> {
    match cfg.auth_ref() {
        Some(name) => match std::env::var(name) {
            Ok(token) if !token.trim().is_empty() => Ok(GithubAuth::Token(token)),
            _ => Err(AdminError::validation(format!(
                "github connector auth_ref '{name}' did not resolve to a token \
                 (set the named secret/env var); refusing to index"
            ))),
        },
        None => match visibility {
            GithubVisibility::Public => Ok(GithubAuth::Unauthenticated),
            GithubVisibility::Private => Err(AdminError::validation(
                "github connector for a private repo requires an 'auth_ref' \
                 naming a token secret",
            )),
        },
    }
}

/// Wraps a connector to override its `name()` with the configured connector
/// name, so the indexing run + its `/admin/indexing/runs` row are keyed by the
/// human-chosen connector name (not the generic `"web"` / `"file"` / `"github"`
/// kind label). Delegates `pull` unchanged.
struct NamedConnector<C: Connector> {
    name: String,
    inner: C,
}

impl<C: Connector> NamedConnector<C> {
    fn new(name: String, inner: C) -> Self {
        Self { name, inner }
    }
}

#[async_trait::async_trait]
impl<C: Connector> Connector for NamedConnector<C> {
    fn name(&self) -> &str {
        &self.name
    }

    async fn pull(
        &self,
        since: Option<smooth_operator_ingestion::Timestamp>,
    ) -> anyhow::Result<Vec<smooth_operator_ingestion::RawDocument>> {
        self.inner.pull(since).await
    }
}

// ---------------------------------------------------------------------------
// Agent settings (Phase 12, increment 3)
// ---------------------------------------------------------------------------

/// The wire body for `PUT /admin/settings`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsWrite {
    model: String,
    system_prompt: String,
    #[serde(default)]
    default_tools: Vec<String>,
}

/// Serialize [`AgentSettings`] under a `settings` key.
fn settings_json(s: &AgentSettings) -> Value {
    serde_json::json!({
        "settings": {
            "orgId": s.org_id,
            "model": s.model,
            "systemPrompt": s.system_prompt,
            "defaultTools": s.default_tools,
            "updatedAt": s.updated_at,
        }
    })
}

/// `GET /admin/settings` — the org's agent settings (defaults if unset). Curator+.
async fn get_settings(
    RequireRole::<1>(principal): RequireRole<1>,
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = state.settings.get(&principal.org_id);
    Json(settings_json(&settings))
}

/// `PUT /admin/settings` — replace the org's agent settings (Admin only).
async fn put_settings(
    RequireRole::<2>(principal): RequireRole<2>,
    State(state): State<AppState>,
    Json(body): Json<SettingsWrite>,
) -> Json<Value> {
    let settings = AgentSettings {
        org_id: principal.org_id.clone(),
        model: body.model,
        system_prompt: body.system_prompt,
        default_tools: body.default_tools,
        updated_at: chrono::Utc::now(),
    };
    state.settings.put(settings.clone());
    Json(settings_json(&settings))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether `user_id` owns the conversation — true when a `User` participant in
/// the conversation carries `external_id == user_id`.
async fn conversation_owned_by(state: &AppState, conversation_id: &str, user_id: &str) -> bool {
    match state
        .storage
        .list_participants_by_conversation(conversation_id)
        .await
    {
        Ok(parts) => parts.iter().any(|p| {
            p.participant_type == ParticipantType::User && p.external_id.as_deref() == Some(user_id)
        }),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Realtime publish — `POST /admin/publish`
// ---------------------------------------------------------------------------

/// A delivery target in the publish request, in a friendlier `{type, id}` shape
/// than [`Target`]'s default enum serialization.
#[derive(Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
enum PublishTarget {
    Connection(String),
    Session(String),
    User(String),
    Org(String),
    Agent(String),
}

impl From<PublishTarget> for Target {
    fn from(t: PublishTarget) -> Self {
        match t {
            PublishTarget::Connection(id) => Target::Connection(id),
            PublishTarget::Session(id) => Target::Session(id),
            PublishTarget::User(id) => Target::User(id),
            PublishTarget::Org(id) => Target::Org(id),
            PublishTarget::Agent(id) => Target::Agent(id),
        }
    }
}

/// `POST /admin/publish` body: the [`PublishTarget`] and the event payload to
/// deliver verbatim to every connection for that target.
#[derive(Deserialize)]
struct PublishRequest {
    target: PublishTarget,
    event: Value,
}

/// `POST /admin/publish` response.
#[derive(Serialize)]
struct PublishResponse {
    /// Connections this **pod** delivered to. With a distributed backplane the
    /// event also fans out to connections on other pods, which this count omits
    /// (each pod delivers to its own sockets) — so `0` here does NOT mean
    /// "delivered to nobody", only "nobody on the pod that served this request".
    delivered: usize,
}

/// Push a realtime event to a backplane target over the WebSocket fleet — the
/// plug point for **non-AI publishers** (job status, ingestion progress,
/// notifications, billing): any service can deliver to a connected client
/// without going through an agent turn.
///
/// Admin-gated. Targets are opaque ids matched against the backplane's
/// connection registry; this layer does not org-validate session/user/agent ids
/// (the backplane is an id-routing layer, not an authz layer). A host that needs
/// hard tenant isolation namespaces those ids in its own wrapper before they
/// reach the backplane. Callers are trusted internal services holding an Admin
/// credential.
async fn publish_event(
    RequireRole::<2>(_principal): RequireRole<2>,
    State(state): State<AppState>,
    Json(body): Json<PublishRequest>,
) -> Json<PublishResponse> {
    let delivered = state
        .backplane
        .publish(body.target.into(), body.event)
        .await;
    Json(PublishResponse { delivered })
}
