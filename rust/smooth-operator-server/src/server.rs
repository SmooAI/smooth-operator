//! The axum WebSocket server: one `/ws` endpoint, one task per connection.
//!
//! Per connection we split the socket and run two tasks joined by an
//! `UnboundedSender<serde_json::Value>` outbound sink:
//!
//! - a **writer** that drains the sink and writes each event as a JSON text
//!   frame, and
//! - a **reader** that reads inbound frames and dispatches them via
//!   [`crate::handler::handle_frame`], passing the sink so handlers (including
//!   the streaming `send_message`) can emit events as they happen.
//!
//! Using a sink channel (instead of writing directly from the handler) is what
//! lets a streaming turn fire many `stream_token` events from inside the agent
//! loop while the connection is still reading.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;

use futures_util::{SinkExt, StreamExt};
use smooth_operator::access_control::AccessContext;
use tokio::net::TcpListener;

use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::{Document, DocumentType};

use crate::config::ServerConfig;
use crate::handler;
use crate::state::AppState;

/// Build the axum [`Router`] for the given application state. Exposed so tests
/// can boot the server in-process. Serves the WebSocket `/ws` endpoint plus the
/// auth-gated admin HTTP API under `/admin` (see [`crate::admin`]).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_upgrade))
        .merge(crate::admin::router())
        .with_state(state)
}

/// The document set the seeded demo docs are tagged into, so
/// `GET /admin/document-sets` has something to report in a seeded server.
const SEED_DOCUMENT_SET: &str = "policies";

/// The org the seeded demo docs + their document-set registry entries belong to.
/// Mirrors the org `handler::handle_create_session` stamps onto reference
/// conversations, so the seeded sets show up for the reference org's admin
/// caller (and ONLY that org — cross-org scoping).
pub const SEED_ORG_ID: &str = "reference-org";

/// Build an [`AppState`] over a fresh in-memory adapter, seeding the knowledge
/// base when `config.seed_kb` is set. Exposed for tests + the binary.
///
/// The auth verifier defaults to [`NoAuthVerifier`](smooth_operator::auth::NoAuthVerifier)
/// here (the protocol-only path needs no auth); the **binary** path
/// ([`build_state_from_env`]) installs the env-configured, secure-by-default
/// verifier instead.
#[must_use]
pub fn build_state(config: ServerConfig) -> AppState {
    let seed = config.seed_kb;
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), config);
    if seed {
        seed_knowledge(storage.as_ref());
        // Record the seeded docs' document-set membership for the admin API
        // (the in-memory backend drops document metadata, so the registry is the
        // source of truth for set names + counts).
        state.record_document_set(SEED_ORG_ID, SEED_DOCUMENT_SET);
        state.record_document_set(SEED_ORG_ID, SEED_DOCUMENT_SET);
    }
    state
}

/// Build an [`AppState`] with the **env-configured** auth verifier (secure by
/// default — see [`smooth_operator::auth::AuthConfig`]). Used by the binary.
///
/// # Errors
/// Returns an error if the auth configuration is invalid (e.g. `AUTH_MODE=jwt`
/// with no key) — the server refuses to start rather than fall back to no-auth.
pub fn build_state_from_env(config: ServerConfig) -> Result<AppState> {
    let verifier = smooth_operator::auth::AuthConfig::from_env()
        .map_err(|e| anyhow::anyhow!("auth configuration error: {e}"))?;
    Ok(build_state(config).with_auth(Arc::from(verifier)))
}

/// Build an [`AppState`] selecting the **storage backend** (and the matching
/// durable **admin stores**) from `config.storage`, then installing the
/// env-configured auth verifier.
///
/// - [`StorageBackend::Memory`](crate::config::StorageBackend::Memory) — the
///   in-memory adapter + in-memory admin stores (the [`build_state`] path; lost
///   on restart). The default.
/// - [`StorageBackend::Postgres`](crate::config::StorageBackend::Postgres) —
///   the Postgres + pgvector adapter; the admin stores persist to the **same
///   database** (`connector_configs` / `agent_settings` / `indexing_runs`).
///   Connection string from `SMOOTH_AGENT_DATABASE_URL` / `DATABASE_URL`.
/// - [`StorageBackend::Dynamodb`](crate::config::StorageBackend::Dynamodb) — the
///   DynamoDB single-table adapter; the admin stores persist to the **same
///   table**. Table from `SMOOTH_AGENT_DDB_TABLE`; the table is created if
///   absent.
///
/// The admin store backend always matches the storage backend so a connector
/// config / settings / indexing run survives a restart wherever the
/// conversations and knowledge live.
///
/// # Errors
/// Returns an error if the auth configuration is invalid, or if the selected
/// persistent backend fails to connect / migrate.
pub async fn build_state_from_env_async(config: ServerConfig) -> Result<AppState> {
    use crate::config::StorageBackend;
    use smooth_operator::adapter::StorageAdapter;

    let verifier = smooth_operator::auth::AuthConfig::from_env()
        .map_err(|e| anyhow::anyhow!("auth configuration error: {e}"))?;

    let state = match config.storage {
        // The in-memory path is unchanged (synchronous, no external services).
        StorageBackend::Memory => build_state(config),

        StorageBackend::Postgres => {
            use smooth_operator_adapter_postgres::PostgresAdapter;
            // The pgvector column width MUST match the embedder the `/index`
            // path uses (1536 keyed / 1024 offline). Build the embedder from
            // config and create the adapter with it so document vectors (at
            // ingest) and query vectors agree — no silent 1024/1536 mismatch.
            let embedder = crate::embedder::build_embedder(
                &crate::embedder::EmbedderConfig::from_server_config(&config),
            );
            let conn_str = std::env::var("SMOOTH_AGENT_DATABASE_URL")
                .or_else(|_| std::env::var("DATABASE_URL"))
                .map_err(|_| {
                    anyhow::anyhow!(
                        "Postgres backend selected but neither SMOOTH_AGENT_DATABASE_URL \
                             nor DATABASE_URL is set"
                    )
                })?;
            let adapter = Arc::new(
                PostgresAdapter::connect_with_embedder(&conn_str, embedder)
                    .await
                    .map_err(|e| anyhow::anyhow!("connecting Postgres storage backend: {e}"))?,
            );
            // Admin stores against the SAME database — durable.
            let connectors = Arc::new(adapter.connector_config_store());
            let settings = Arc::new(adapter.settings_store());
            let indexing = Arc::new(adapter.indexing_store());
            let storage: Arc<dyn StorageAdapter> = adapter;
            AppState::new(storage, config)
                .with_connector_configs(connectors)
                .with_settings(settings)
                .with_indexing(indexing)
        }

        StorageBackend::Dynamodb => {
            use smooth_operator_adapter_dynamodb::DynamoDbAdapter;
            let adapter = Arc::new(
                DynamoDbAdapter::from_env(None)
                    .await
                    .map_err(|e| anyhow::anyhow!("connecting DynamoDB storage backend: {e}"))?,
            );
            adapter
                .create_table()
                .await
                .map_err(|e| anyhow::anyhow!("creating DynamoDB table: {e}"))?;
            // Admin stores against the SAME table — durable.
            let connectors = Arc::new(adapter.connector_config_store());
            let settings = Arc::new(adapter.settings_store());
            let indexing = Arc::new(adapter.indexing_store());
            let storage: Arc<dyn StorageAdapter> = adapter;
            AppState::new(storage, config)
                .with_connector_configs(connectors)
                .with_settings(settings)
                .with_indexing(indexing)
        }
    };

    Ok(state.with_auth(Arc::from(verifier)))
}

/// Seed a couple of distinctive demo docs so knowledge-grounded E2E is
/// deterministic. The 17-day return window is deliberately unusual so an
/// ungrounded answer can't accidentally match it. Both docs are tagged into the
/// `policies` document set so the admin API can report it.
pub fn seed_knowledge(storage: &InMemoryStorageAdapter) {
    let kb = smooth_operator::adapter::StorageAdapter::knowledge(storage);
    let _ = kb.ingest(smooth_operator::with_document_set(
        Document::new(
            "SmooAI's return window is exactly 17 days from delivery. Returns after 17 days are not accepted.",
            "policies/returns.md",
            DocumentType::Documentation,
        ),
        [SEED_DOCUMENT_SET],
    ));
    let _ = kb.ingest(smooth_operator::with_document_set(
        Document::new(
            "SmooAI standard shipping takes 5 to 7 business days. Expedited shipping takes 2 business days.",
            "policies/shipping.md",
            DocumentType::Documentation,
        ),
        [SEED_DOCUMENT_SET],
    ));
}

/// Bind on `<SMOOTH_AGENT_BIND>:<port>` (default loopback) and serve until the
/// process is killed. Returns the bound [`TcpListener`] + the router, used by
/// both the binary and tests (tests bind port 0 for an ephemeral port).
///
/// Uses the **env-configured, secure-by-default** auth verifier
/// ([`build_state_from_env`]) — the binary refuses to start if auth is
/// misconfigured rather than silently serving the admin API unauthenticated.
///
/// # Errors
/// Returns an error if the auth configuration is invalid or the TCP bind fails.
pub async fn bind(config: ServerConfig) -> Result<(TcpListener, Router)> {
    let ip: std::net::IpAddr = config
        .bind
        .parse()
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let addr = SocketAddr::new(ip, config.port);
    // Async so a Postgres / DynamoDB storage backend (and its matching durable
    // admin stores) can be wired; in-memory stays synchronous inside.
    let state = build_state_from_env_async(config).await?;
    let app = router(state);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding WebSocket server on {addr}"))?;
    Ok((listener, app))
}

/// Serve a **pre-built** [`AppState`] to completion (blocks), binding on
/// `state.config.bind:state.config.port`.
///
/// This is the library entry point for callers that assemble their own
/// `AppState` — e.g. the `dev-support` example, which ingests a GitHub repo into
/// a storage adapter, wires the env-configured [`AuthVerifier`], and then serves
/// that exact state so the chat-widget queries the ingested knowledge. It does
/// **not** rebuild the state or touch the ACL/auth/embedder/reranker selection —
/// those are baked into the `state` the caller passes in. The WS loop, router,
/// and listening log are identical to [`run`] (which builds its state from env);
/// `run` is unchanged.
///
/// # Errors
/// Returns an error if the TCP bind fails or serving fails.
pub async fn serve_state(state: AppState) -> Result<()> {
    let ip: std::net::IpAddr = state
        .config
        .bind
        .parse()
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let addr = SocketAddr::new(ip, state.config.port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding WebSocket server on {addr}"))?;
    serve_state_on(state, listener).await
}

/// Serve a pre-built [`AppState`] on an already-bound [`TcpListener`] (blocks).
///
/// Splitting the bind from the serve lets a caller (or a test) bind an ephemeral
/// port, read [`TcpListener::local_addr`] for the real port, then hand the
/// listener here. Logs the same "listening" line [`run`] does.
///
/// # Errors
/// Returns an error if serving fails.
pub async fn serve_state_on(state: AppState, listener: TcpListener) -> Result<()> {
    let has_llm = state.config.has_llm();
    let model = state.config.model.clone();
    let gateway = state.config.gateway_url.clone();
    let local = listener.local_addr().context("local addr")?;
    let app = router(state);

    tracing::info!(
        %local,
        endpoint = "/ws",
        %model,
        %gateway,
        llm_enabled = has_llm,
        "smooth-operator-server listening"
    );
    println!(
        "smooth-operator-server listening on ws://{local}/ws (model={model}, llm_enabled={has_llm})"
    );

    axum::serve(listener, app)
        .await
        .context("serving WebSocket connections")?;
    Ok(())
}

/// Run the server to completion (blocks). Logs a single listening line.
///
/// # Errors
/// Returns an error if binding or serving fails.
pub async fn run(config: ServerConfig) -> Result<()> {
    let has_llm = config.has_llm();
    let model = config.model.clone();
    let gateway = config.gateway_url.clone();
    let (listener, app) = bind(config).await?;
    let local = listener.local_addr().context("local addr")?;

    tracing::info!(
        %local,
        endpoint = "/ws",
        %model,
        %gateway,
        llm_enabled = has_llm,
        "smooth-operator-server listening"
    );
    // Also print to stdout so the run-confirmation check is unambiguous without
    // a tracing subscriber filter.
    println!(
        "smooth-operator-server listening on ws://{local}/ws (model={model}, llm_enabled={has_llm})"
    );

    axum::serve(listener, app)
        .await
        .context("serving WebSocket connections")?;
    Ok(())
}

/// Query parameters accepted on the `/ws` upgrade. `token` carries the bearer
/// JWT used to authenticate the connection (browsers can't set custom headers on
/// a WebSocket handshake, so the token rides on the query string — the standard
/// pattern for WS auth).
#[derive(Debug, serde::Deserialize, Default)]
struct WsQuery {
    /// The bearer token (raw JWT, no `Bearer ` prefix), if provided.
    #[serde(default)]
    token: Option<String>,
}

/// Resolve the connection's [`AccessContext`] from the `?token=` query param.
///
/// **Fail closed for ACL'd content**: when no token is presented, or the auth
/// verifier is the no-key [`AdminDisabledVerifier`] (admin/auth unconfigured —
/// dev/no-auth), or the token fails to verify, the connection runs as
/// [`AccessContext::anonymous`] — which sees **only org-public** knowledge, not
/// every document. A valid token yields the principal's full
/// [`AccessContext`](smooth_operator::auth::Principal::access_context) (user id +
/// groups), so it can read documents scoped to it. Verification failures are
/// logged (never the token) and degrade to anonymous rather than dropping the
/// connection, so the dev/no-auth case still serves org-public knowledge.
fn resolve_ws_access(state: &AppState, query: &WsQuery) -> AccessContext {
    let Some(token) = query
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        // No token → anonymous (org-public only). Keeps the dev/no-auth `/ws`
        // path working while failing closed for ACL'd content.
        return AccessContext::anonymous();
    };
    match state.auth.verify(token) {
        Ok(principal) => principal.access_context(),
        Err(e) => {
            // Don't leak the token; log only the mode + a generic reason.
            tracing::warn!(
                auth_mode = state.auth.mode(),
                error = %e,
                "ws token failed verification; serving org-public knowledge only (anonymous)"
            );
            AccessContext::anonymous()
        }
    }
}

/// Axum handler: upgrade an HTTP request on `/ws` to a WebSocket. The bearer
/// token (if any) is taken from the `?token=` query param, resolved to an
/// [`AccessContext`] at connect time, and threaded into every turn so retrieval
/// is access-controlled per connection.
async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let access = resolve_ws_access(&state, &query);
    // Capture the browser's `Origin` at the handshake (browsers always send it,
    // and can't be made to forge another site's). It's enforced per-agent at
    // session creation against the agent's embed allowlist (widget_auth).
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    ws.on_upgrade(move |socket| connection_loop(socket, state, access, origin))
}

/// Drive one WebSocket connection: split into reader + writer, joined by an
/// outbound event sink. `access` is the connection's resolved document-level
/// entitlement, threaded into every `send_message` turn. `origin` is the
/// handshake `Origin` header, enforced against an agent's embed allowlist.
async fn connection_loop(
    socket: WebSocket,
    state: AppState,
    access: AccessContext,
    origin: Option<String>,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();

    // Register this connection's outbound sink with the backplane so events
    // published from anywhere (this pod or, with a Redis/NATS impl, another) can
    // reach it. `conn_id` is associated with its session at create-session time.
    let conn_id = uuid::Uuid::new_v4().to_string();
    let sink_for_backplane = sink_tx.clone();
    state
        .backplane
        .attach(
            &conn_id,
            std::sync::Arc::new(move |event| {
                let _ = sink_for_backplane.send(event);
            }),
        )
        .await;

    // Writer: drain the sink and write each event as a JSON text frame.
    let writer = tokio::spawn(async move {
        while let Some(event) = sink_rx.recv().await {
            let text = match serde_json::to_string(&event) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ws_tx.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Reader: dispatch inbound frames. Handlers emit events via `sink_tx`.
    while let Some(frame) = ws_rx.next().await {
        match frame {
            Ok(Message::Text(text)) => {
                handler::handle_frame(
                    &state,
                    &access,
                    &conn_id,
                    origin.as_deref(),
                    text.as_str(),
                    &sink_tx,
                )
                .await;
            }
            Ok(Message::Binary(_)) => {
                let _ = sink_tx.send(crate::protocol::error(
                    None,
                    "VALIDATION_ERROR",
                    "binary frames are not supported; send JSON text frames",
                ));
            }
            Ok(Message::Close(_)) => break,
            // Ping/Pong control frames are handled by axum automatically.
            Ok(_) => {}
            Err(_) => break,
        }
    }

    // Reader finished → detach from the backplane, then drop the sink so the
    // writer task exits.
    state.backplane.detach(&conn_id).await;
    drop(sink_tx);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use smooth_operator::adapter::StorageAdapter;

    #[test]
    fn seeded_kb_returns_17_day_fact() {
        let storage = InMemoryStorageAdapter::new();
        seed_knowledge(&storage);
        let results = storage
            .knowledge()
            .query("return window policy", 3)
            .expect("query");
        assert!(
            results.iter().any(|r| r.chunk.contains("17")),
            "expected seeded 17-day fact, got: {results:?}"
        );
    }

    #[tokio::test]
    async fn build_state_without_key_has_no_llm() {
        let cfg = ServerConfig {
            bind: "127.0.0.1".into(),
            port: 0,
            gateway_url: "https://example.test/v1".into(),
            gateway_key: None,
            model: "m".into(),
            seed_kb: true,
            max_iterations: 4,
            max_tokens: 128,
            storage: crate::config::StorageBackend::Memory,
            widget_auth_strict: false,
        };
        let state = build_state(cfg);
        assert!(!state.config.has_llm());
    }
}
