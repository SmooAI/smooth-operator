//! Persistent admin stores (Phase 12 follow-up) — Postgres-backed.
//!
//! The three management-console stores ship with process-local in-memory
//! implementations (`InMemoryConnectorConfigStore`, `InMemorySettingsStore`,
//! `InMemoryIndexingStore`) that lose everything on restart. This module makes
//! them durable against the same Postgres the rest of the adapter dogfoods,
//! preserving the in-memory semantics exactly:
//!
//! - [`PgConnectorConfigStore`] — org-scoped CRUD over `connector_configs`
//!   (PK `(org_id, id)`). `list` is sorted by `(name, id)`; cross-org `get` /
//!   `delete` never touch another org's row.
//! - [`PgSettingsStore`] — per-org `agent_settings` (PK `org_id`); `get` of an
//!   unset org returns [`AgentSettings::defaults`], `put` is an upsert.
//! - [`PgIndexingStore`] — the `indexing_runs` ledger (PK `id`). `record_run`
//!   upserts by id (so a `Running` row can be promoted to a terminal state),
//!   `list_runs` returns a connector's runs oldest-first, and `latest_cursor`
//!   is `max(cursor)` over **succeeded** runs only — a failed run never advances
//!   the cursor.
//!
//! ## Sync trait over an async pool
//!
//! All three store traits are **synchronous** (the engine / admin API call them
//! directly), but `deadpool` is async. We bridge with the same
//! [`run_blocking`](run_blocking) helper the knowledge base uses: `spawn` the
//! async work onto a captured runtime [`Handle`] (so its I/O makes progress on
//! that runtime's reactor) and block the calling thread on the `JoinHandle` from
//! a throwaway OS thread — never `Handle::block_on` on a runtime worker thread.

use std::future::Future;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use tokio::runtime::Handle;

use smooth_operator::connector_config::{ConnectorConfig, ConnectorConfigStore, ConnectorKind};
use smooth_operator::settings::{AgentSettings, SettingsStore};
use smooth_operator_ingestion::indexing::{IndexingRun, IndexingRunStatus, IndexingStore};
use smooth_operator_ingestion::Timestamp;

/// Drive an async future to completion from a *synchronous* trait method.
///
/// Identical bridge to `PgKnowledgeBase::run_blocking`: `spawn` onto the
/// captured runtime so the async I/O makes progress on that runtime's reactor,
/// then block on the `JoinHandle` from a throwaway OS thread running a tiny
/// current-thread runtime. This never calls `Handle::block_on` on a runtime
/// worker thread (which panics "Cannot start a runtime from within a runtime"),
/// so it is safe whether the caller is on a worker or a plain OS thread.
fn run_blocking<F, T>(handle: &Handle, fut: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let join = handle.spawn(fut);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> Result<T> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let joined = rt.block_on(join);
            joined.map_err(|e| anyhow!("admin store task panicked or was cancelled: {e}"))?
        })();
        let _ = tx.send(result);
    });
    rx.recv()
        .map_err(|e| anyhow!("admin store task channel closed: {e}"))?
}

// ---------------------------------------------------------------------------
// Connector config store
// ---------------------------------------------------------------------------

/// Postgres-backed [`ConnectorConfigStore`] over `connector_configs`.
#[derive(Clone)]
pub struct PgConnectorConfigStore {
    pool: Pool,
    handle: Handle,
}

impl PgConnectorConfigStore {
    /// Build over the adapter's async pool + captured runtime handle.
    #[must_use]
    pub fn new(pool: Pool, handle: Handle) -> Self {
        Self { pool, handle }
    }

    async fn list_async(&self, org_id: String) -> Result<Vec<ConnectorConfig>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT id, org_id, name, kind, config, enabled, created_at, updated_at
                 FROM connector_configs
                 WHERE org_id = $1
                 ORDER BY name, id",
                &[&org_id],
            )
            .await?;
        rows.iter().map(row_to_connector).collect()
    }

    async fn get_async(&self, org_id: String, id: String) -> Result<Option<ConnectorConfig>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT id, org_id, name, kind, config, enabled, created_at, updated_at
                 FROM connector_configs
                 WHERE org_id = $1 AND id = $2",
                &[&org_id, &id],
            )
            .await?;
        row.as_ref().map(row_to_connector).transpose()
    }

    async fn upsert_async(&self, cfg: ConnectorConfig) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO connector_configs
                    (org_id, id, name, kind, config, enabled, created_at, updated_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
                 ON CONFLICT (org_id, id) DO UPDATE SET
                    name       = EXCLUDED.name,
                    kind       = EXCLUDED.kind,
                    config     = EXCLUDED.config,
                    enabled    = EXCLUDED.enabled,
                    created_at = EXCLUDED.created_at,
                    updated_at = EXCLUDED.updated_at",
                &[
                    &cfg.org_id,
                    &cfg.id,
                    &cfg.name,
                    &cfg.kind.as_str(),
                    &cfg.config,
                    &cfg.enabled,
                    &cfg.created_at,
                    &cfg.updated_at,
                ],
            )
            .await?;
        Ok(())
    }

    async fn delete_async(&self, org_id: String, id: String) -> Result<bool> {
        let client = self.pool.get().await?;
        let n = client
            .execute(
                "DELETE FROM connector_configs WHERE org_id = $1 AND id = $2",
                &[&org_id, &id],
            )
            .await?;
        Ok(n > 0)
    }
}

fn row_to_connector(row: &tokio_postgres::Row) -> Result<ConnectorConfig> {
    let kind_str: String = row.get("kind");
    let kind = ConnectorKind::parse(&kind_str)
        .map_err(|bad| anyhow!("unknown connector kind '{bad}' in connector_configs row"))?;
    Ok(ConnectorConfig {
        id: row.get("id"),
        org_id: row.get("org_id"),
        name: row.get("name"),
        kind,
        config: row.get("config"),
        enabled: row.get("enabled"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

impl ConnectorConfigStore for PgConnectorConfigStore {
    fn list(&self, org_id: &str) -> Vec<ConnectorConfig> {
        let this = self.clone();
        let org_id = org_id.to_string();
        run_blocking(&self.handle, async move { this.list_async(org_id).await }).unwrap_or_default()
    }

    fn get(&self, org_id: &str, id: &str) -> Option<ConnectorConfig> {
        let this = self.clone();
        let org_id = org_id.to_string();
        let id = id.to_string();
        run_blocking(
            &self.handle,
            async move { this.get_async(org_id, id).await },
        )
        .ok()
        .flatten()
    }

    fn upsert(&self, config: ConnectorConfig) {
        let this = self.clone();
        let _ = run_blocking(&self.handle, async move { this.upsert_async(config).await });
    }

    fn delete(&self, org_id: &str, id: &str) -> bool {
        let this = self.clone();
        let org_id = org_id.to_string();
        let id = id.to_string();
        run_blocking(
            &self.handle,
            async move { this.delete_async(org_id, id).await },
        )
        .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Settings store
// ---------------------------------------------------------------------------

/// Postgres-backed [`SettingsStore`] over `agent_settings`.
#[derive(Clone)]
pub struct PgSettingsStore {
    pool: Pool,
    handle: Handle,
}

impl PgSettingsStore {
    /// Build over the adapter's async pool + captured runtime handle.
    #[must_use]
    pub fn new(pool: Pool, handle: Handle) -> Self {
        Self { pool, handle }
    }

    async fn get_async(&self, org_id: String) -> Result<Option<AgentSettings>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT org_id, model, system_prompt, default_tools, updated_at
                 FROM agent_settings WHERE org_id = $1",
                &[&org_id],
            )
            .await?;
        match row {
            Some(row) => {
                let default_tools: serde_json::Value = row.get("default_tools");
                let default_tools: Vec<String> = serde_json::from_value(default_tools)?;
                Ok(Some(AgentSettings {
                    org_id: row.get("org_id"),
                    model: row.get("model"),
                    system_prompt: row.get("system_prompt"),
                    default_tools,
                    updated_at: row.get("updated_at"),
                }))
            }
            None => Ok(None),
        }
    }

    async fn put_async(&self, settings: AgentSettings) -> Result<()> {
        let client = self.pool.get().await?;
        let default_tools = serde_json::to_value(&settings.default_tools)?;
        client
            .execute(
                "INSERT INTO agent_settings
                    (org_id, model, system_prompt, default_tools, updated_at)
                 VALUES ($1,$2,$3,$4,$5)
                 ON CONFLICT (org_id) DO UPDATE SET
                    model         = EXCLUDED.model,
                    system_prompt = EXCLUDED.system_prompt,
                    default_tools = EXCLUDED.default_tools,
                    updated_at    = EXCLUDED.updated_at",
                &[
                    &settings.org_id,
                    &settings.model,
                    &settings.system_prompt,
                    &default_tools,
                    &settings.updated_at,
                ],
            )
            .await?;
        Ok(())
    }
}

impl SettingsStore for PgSettingsStore {
    fn get(&self, org_id: &str) -> AgentSettings {
        let this = self.clone();
        let org = org_id.to_string();
        // Absent row (or a transient read failure) falls back to defaults so the
        // console always has a populated form, matching the in-memory store.
        run_blocking(&self.handle, async move { this.get_async(org).await })
            .ok()
            .flatten()
            .unwrap_or_else(|| AgentSettings::defaults(org_id))
    }

    fn put(&self, settings: AgentSettings) {
        let this = self.clone();
        let _ = run_blocking(&self.handle, async move { this.put_async(settings).await });
    }
}

// ---------------------------------------------------------------------------
// Indexing store
// ---------------------------------------------------------------------------

/// Postgres-backed [`IndexingStore`] over `indexing_runs`.
#[derive(Clone)]
pub struct PgIndexingStore {
    pool: Pool,
    handle: Handle,
}

impl PgIndexingStore {
    /// Build over the adapter's async pool + captured runtime handle.
    #[must_use]
    pub fn new(pool: Pool, handle: Handle) -> Self {
        Self { pool, handle }
    }

    async fn record_run_async(&self, run: IndexingRun) -> Result<()> {
        let client = self.pool.get().await?;
        let status = status_to_str(run.status);
        let documents_seen = i64::try_from(run.documents_seen).unwrap_or(i64::MAX);
        let chunks_indexed = i64::try_from(run.chunks_indexed).unwrap_or(i64::MAX);
        let documents_skipped = i64::try_from(run.documents_skipped).unwrap_or(i64::MAX);
        client
            .execute(
                "INSERT INTO indexing_runs
                    (id, connector_name, status, started_at, finished_at,
                     documents_seen, chunks_indexed, documents_skipped, cursor, error)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
                 ON CONFLICT (id) DO UPDATE SET
                    connector_name    = EXCLUDED.connector_name,
                    status            = EXCLUDED.status,
                    started_at        = EXCLUDED.started_at,
                    finished_at       = EXCLUDED.finished_at,
                    documents_seen    = EXCLUDED.documents_seen,
                    chunks_indexed    = EXCLUDED.chunks_indexed,
                    documents_skipped = EXCLUDED.documents_skipped,
                    cursor            = EXCLUDED.cursor,
                    error             = EXCLUDED.error",
                &[
                    &run.id,
                    &run.connector_name,
                    &status,
                    &run.started_at,
                    &run.finished_at,
                    &documents_seen,
                    &chunks_indexed,
                    &documents_skipped,
                    &run.cursor,
                    &run.error,
                ],
            )
            .await?;
        Ok(())
    }

    async fn latest_cursor_async(&self, connector_name: String) -> Result<Option<Timestamp>> {
        let client = self.pool.get().await?;
        // Max cursor over SUCCEEDED runs only — a failed run never advances it.
        let row = client
            .query_one(
                "SELECT max(cursor) AS c
                 FROM indexing_runs
                 WHERE connector_name = $1 AND status = 'succeeded'",
                &[&connector_name],
            )
            .await?;
        Ok(row.get::<_, Option<DateTime<Utc>>>("c"))
    }

    async fn list_runs_async(&self, connector_name: String) -> Result<Vec<IndexingRun>> {
        let client = self.pool.get().await?;
        // Oldest-first to match the in-memory insertion-order contract.
        let rows = client
            .query(
                "SELECT id, connector_name, status, started_at, finished_at,
                        documents_seen, chunks_indexed, documents_skipped, cursor, error
                 FROM indexing_runs
                 WHERE connector_name = $1
                 ORDER BY started_at ASC, id ASC",
                &[&connector_name],
            )
            .await?;
        rows.iter().map(row_to_run).collect()
    }
}

fn status_to_str(status: IndexingRunStatus) -> &'static str {
    match status {
        IndexingRunStatus::Running => "running",
        IndexingRunStatus::Succeeded => "succeeded",
        IndexingRunStatus::Failed => "failed",
    }
}

fn status_from_str(s: &str) -> Result<IndexingRunStatus> {
    Ok(match s {
        "running" => IndexingRunStatus::Running,
        "succeeded" => IndexingRunStatus::Succeeded,
        "failed" => IndexingRunStatus::Failed,
        other => return Err(anyhow!("unknown indexing run status '{other}'")),
    })
}

fn row_to_run(row: &tokio_postgres::Row) -> Result<IndexingRun> {
    let status = status_from_str(row.get::<_, String>("status").as_str())?;
    let documents_seen: i64 = row.get("documents_seen");
    let chunks_indexed: i64 = row.get("chunks_indexed");
    let documents_skipped: i64 = row.get("documents_skipped");
    Ok(IndexingRun {
        id: row.get("id"),
        connector_name: row.get("connector_name"),
        status,
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
        documents_seen: usize::try_from(documents_seen).unwrap_or(0),
        chunks_indexed: usize::try_from(chunks_indexed).unwrap_or(0),
        documents_skipped: usize::try_from(documents_skipped).unwrap_or(0),
        cursor: row.get("cursor"),
        error: row.get("error"),
    })
}

impl IndexingStore for PgIndexingStore {
    fn record_run(&self, run: &IndexingRun) {
        let this = self.clone();
        let run = run.clone();
        let _ = run_blocking(
            &self.handle,
            async move { this.record_run_async(run).await },
        );
    }

    fn latest_cursor(&self, connector_name: &str) -> Option<Timestamp> {
        let this = self.clone();
        let name = connector_name.to_string();
        run_blocking(
            &self.handle,
            async move { this.latest_cursor_async(name).await },
        )
        .ok()
        .flatten()
    }

    fn list_runs(&self, connector_name: &str) -> Vec<IndexingRun> {
        let this = self.clone();
        let name = connector_name.to_string();
        run_blocking(
            &self.handle,
            async move { this.list_runs_async(name).await },
        )
        .unwrap_or_default()
    }
}
