//! The `serve` step: ingest the configured repo, then run the real
//! `smooth-operator-server` over the ingested knowledge so the embeddable
//! chat-widget can connect and chat — grounded in the repo.
//!
//! Unlike `ingest` (which prints a report and exits) and `chat` (a terminal
//! REPL), `serve` is the **full-page chat-widget** path:
//!
//! 1. Build the GitHub connector and run the ingestion pipeline (chunk → embed →
//!    store) into a storage adapter — the *same* connector + pipeline `ingest`
//!    and `chat` use. The embedder is selected by [`build_embedder`]: the real
//!    semantic `GatewayEmbedder` when `SMOOAI_GATEWAY_KEY` is set, else the
//!    network-free deterministic fallback. Storage is Postgres (pgvector) when
//!    `SMOOTH_AGENT_STORAGE=postgres` + a DB URL is configured, else in-memory.
//! 2. Build the WebSocket server's [`AppState`] over that pre-populated storage,
//!    the resolved [`ServerConfig`] (gateway/model/limits), and the
//!    env-configured [`AuthVerifier`](smooth_operator::auth::AuthVerifier) — the
//!    server already enforces the per-connection ACL on retrieval.
//! 3. Print a ready banner and call
//!    [`serve_state`](smooth_operator_server::serve_state) — we do **not**
//!    reimplement the WS loop; the server crate owns it.
//!
//! Secrets come from the environment only (`SMOOAI_GATEWAY_KEY`, `GITHUB_TOKEN`,
//! the `AUTH_*` vars). `AUTH_MODE=none` (the local-dev default story) serves
//! org-public knowledge to anonymous widget connections.

use std::sync::Arc;

use anyhow::{Context, Result};

use smooth_operator::adapter::StorageAdapter;
use smooth_operator::embedding::Embedder;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_ingestion::IngestReport;
use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::embedder::build_embedder;
use smooth_operator_server::state::AppState;

use crate::config::DevSupportConfig;
use crate::ingest::{build_connector, embedder_config_from_env, ingest_into_with_embedder};

/// The fully-prepared, ready-to-serve state for the dev-support widget server:
/// an [`AppState`] whose storage already holds the ingested repo, plus the
/// [`IngestReport`] (for the ready banner) and the org id the docs were scoped
/// to.
pub struct ServeState {
    /// The server state to hand to [`serve_state`](smooth_operator_server::serve_state).
    pub app_state: AppState,
    /// What the on-boot ingest pulled/stored (for the banner).
    pub report: IngestReport,
    /// The org id the repo's documents were scoped to (the repo slug).
    pub org_id: String,
}

/// Build the storage adapter `serve` ingests into (and the embedder both the
/// ingest and any vector column must share), selected from
/// `server_config.storage`:
///
/// - [`StorageBackend::Memory`] — a fresh [`InMemoryStorageAdapter`] (the local
///   demo default; the index is lost on exit).
/// - [`StorageBackend::Postgres`] — a `PostgresAdapter` opened with the **same**
///   embedder the ingest uses (so document and query vectors share a dimension),
///   from `SMOOTH_AGENT_DATABASE_URL` / `DATABASE_URL`. The index persists.
/// - [`StorageBackend::Dynamodb`] — not wired for this example's `serve`; falls
///   back to in-memory with a warning (the example's showcase is local).
///
/// Returns the adapter plus the embedder instance so the caller ingests with the
/// exact same embedder selection (no second env read, no dimension drift).
///
/// # Errors
/// Propagates Postgres connection / migration failures.
async fn build_storage(
    server_config: &ServerConfig,
) -> Result<(Arc<dyn StorageAdapter>, Arc<dyn Embedder>)> {
    let embedder = build_embedder(&embedder_config_from_env());
    match server_config.storage {
        StorageBackend::Memory => Ok((Arc::new(InMemoryStorageAdapter::new()), embedder)),
        StorageBackend::Postgres => {
            use smooth_operator_adapter_postgres::PostgresAdapter;
            let conn_str = std::env::var("SMOOTH_AGENT_DATABASE_URL")
                .or_else(|_| std::env::var("DATABASE_URL"))
                .map_err(|_| {
                    anyhow::anyhow!(
                        "SMOOTH_AGENT_STORAGE=postgres but neither SMOOTH_AGENT_DATABASE_URL \
                         nor DATABASE_URL is set — export a connection string or use the \
                         in-memory default"
                    )
                })?;
            let adapter = PostgresAdapter::connect_with_embedder(&conn_str, Arc::clone(&embedder))
                .await
                .map_err(|e| anyhow::anyhow!("connecting Postgres storage backend: {e}"))?;
            Ok((Arc::new(adapter), embedder))
        }
        StorageBackend::Dynamodb => {
            tracing::warn!(
                "SMOOTH_AGENT_STORAGE=dynamodb is not wired for the dev-support `serve` example; \
                 falling back to in-memory (the local-demo default)"
            );
            Ok((Arc::new(InMemoryStorageAdapter::new()), embedder))
        }
    }
}

/// Ingest the configured repo, then assemble the ready-to-serve [`AppState`].
///
/// This is the file-free seam the smoke test drives (with a `MockConnector` via
/// [`build_serve_state_with_storage`]); the `serve` CLI command calls
/// [`build_serve_state`], which builds a real [`GithubConnector`] from config.
///
/// The returned [`AppState`] carries the env-configured auth verifier (so the
/// server's ACL/admin path is wired exactly as the binary's), the resolved
/// [`ServerConfig`], and the storage holding the ingested knowledge.
///
/// # Errors
/// Propagates connector pull / ingest errors, Postgres connection errors, and
/// auth-configuration errors.
pub async fn build_serve_state(config: &DevSupportConfig) -> Result<ServeState> {
    let server_config = ServerConfig::from_env();
    let (storage, embedder) = build_storage(&server_config).await?;
    let connector = build_connector(config)?;
    build_serve_state_with_storage(
        config,
        server_config,
        storage,
        embedder.as_ref(),
        &connector,
    )
    .await
}

/// As [`build_serve_state`] but with a caller-supplied storage adapter,
/// [`ServerConfig`], embedder, and
/// [`Connector`](smooth_operator_ingestion::Connector) — the offline test seam
/// (a `MockConnector` + in-memory storage + the deterministic embedder + a
/// key-less config, all without touching the environment for ingest).
///
/// Wires the env-configured auth verifier onto the state via
/// [`AppState::with_auth`], so retrieval is access-controlled exactly as in
/// production. With `AUTH_MODE=none` (or unset) this serves org-public docs to
/// anonymous connections, which is what the local widget demo wants.
///
/// # Errors
/// Propagates ingest errors and auth-configuration errors (`AUTH_MODE=jwt` with
/// no key, etc.).
pub async fn build_serve_state_with_storage(
    config: &DevSupportConfig,
    server_config: ServerConfig,
    storage: Arc<dyn StorageAdapter>,
    embedder: &dyn Embedder,
    connector: &dyn smooth_operator_ingestion::Connector,
) -> Result<ServeState> {
    let org_id = config.org_id();

    // Ingest the repo into the (possibly persistent) storage adapter — the same
    // pipeline `ingest`/`chat` run, only the StorageAdapter differs. The embedder
    // is the same instance the storage was opened with (no dimension drift).
    let report = ingest_into_with_embedder(connector, Arc::clone(&storage), &org_id, embedder)
        .await
        .context("ingesting the configured repo for serve")?;

    // The env-configured, secure-by-default auth verifier — identical to the
    // server binary's `build_state_from_env`. We do not weaken it; `AUTH_MODE`
    // governs it from the environment.
    let verifier = smooth_operator::auth::AuthConfig::from_env()
        .map_err(|e| anyhow::anyhow!("auth configuration error: {e}"))?;

    let app_state = AppState::new(storage, server_config).with_auth(Arc::from(verifier));

    Ok(ServeState {
        app_state,
        report,
        org_id,
    })
}

/// Print the ready banner: the `ws://host:port/ws` endpoint and a one-liner on
/// pointing the embeddable chat-widget at it.
pub fn print_ready_banner(config: &DevSupportConfig, serve: &ServeState) {
    let slug = config.repo_slug();
    let cfg = &serve.app_state.config;
    let host = if cfg.bind == "0.0.0.0" {
        "127.0.0.1"
    } else {
        cfg.bind.as_str()
    };
    let endpoint = format!("ws://{host}:{}/ws", cfg.port);
    let auth_mode = std::env::var("AUTH_MODE")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".to_string());

    println!();
    println!("✓ dev-support is serving {slug}");
    println!(
        "  ingested:  {} docs, {} chunks (embedding dim {}){}",
        serve.report.documents_pulled,
        serve.report.chunks_stored,
        serve.report.embedding_dim,
        if cfg.has_llm() {
            ""
        } else {
            " — no SMOOAI_GATEWAY_KEY: send_message returns a clean LLM_UNAVAILABLE error"
        }
    );
    if matches!(cfg.storage, StorageBackend::Memory) {
        println!("  storage:   in-memory (this demo; the index is gone on exit)");
    } else {
        println!("  storage:   persistent (survives restarts)");
    }
    println!();
    println!("  WebSocket: {endpoint}");
    println!();
    println!("Point the chat-widget (@smooai/chat-widget) at it — full-page mode:");
    println!();
    println!("  <smoo-chat-widget mode=\"fullpage\" endpoint=\"{endpoint}\"></smoo-chat-widget>");
    println!();
    println!(
        "AUTH_MODE={auth_mode} is the local-dev default (org-public): anonymous widget \
         connections see the repo's org-public knowledge. Set AUTH_MODE=jwt + a key to gate it."
    );
    println!();
    println!("Press Ctrl-C to stop.");
}

/// `serve`: ingest the configured repo, print the ready banner, and run the
/// smooth-operator WebSocket server over the ingested knowledge until killed.
///
/// # Errors
/// Propagates ingest, auth-configuration, bind, and serve errors.
pub async fn run_serve(config: &DevSupportConfig) -> Result<()> {
    let slug = config.repo_slug();
    eprintln!("==> Ingesting {slug} for serve (prose+code+issues) …");
    let serve = build_serve_state(config).await?;
    print_ready_banner(config, &serve);

    // Hand the pre-built state to the server's own serve loop — no WS
    // reimplementation here.
    smooth_operator_server::serve_state(serve.app_state).await
}
