//! Persistent admin stores (Phase 12 follow-up) — DynamoDB-backed.
//!
//! The three management-console stores ship with process-local in-memory
//! implementations that lose everything on restart. This module makes them
//! durable against the same single DynamoDB table the rest of the adapter uses,
//! preserving the in-memory semantics exactly:
//!
//! - [`DynamoConnectorConfigStore`] — connector configs at
//!   `PK = ORG#<org>`, `SK = CONNECTOR#<id>`. `list(org)` is a single partition
//!   query (sorted by name in code, matching the in-memory `(name, id)` order);
//!   cross-org `get` / `delete` never touch another org's row.
//! - [`DynamoSettingsStore`] — per-org agent settings at `PK = ORG#<org>`,
//!   `SK = SETTINGS#` (a singleton). `get` of an unset org returns
//!   [`AgentSettings::defaults`]; `put` is a `PutItem` upsert.
//! - [`DynamoIndexingStore`] — the indexing-run ledger at
//!   `PK = IXCONN#<connector_name>`, `SK = <zero-padded started_at>#<id>`.
//!   `record_run` is a `PutItem` upsert by id (the SK embeds the run id so a
//!   `Running` row promotes to a terminal state in place — its `started_at`
//!   doesn't change, so the SK is stable). `list_runs` queries the partition
//!   ascending (oldest-first); `latest_cursor` is `max(cursor)` over
//!   **succeeded** runs only — a failed run never advances it.
//!
//! ## Sync trait over an async SDK
//!
//! All three store traits are **synchronous**; `aws-sdk-dynamodb` is async. We
//! bridge with the same [`run_blocking`] helper the checkpoint store uses:
//! `spawn` the async work onto a captured runtime [`Handle`] and block the
//! calling thread on the `JoinHandle` from a throwaway OS thread — never
//! `Handle::block_on` on a runtime worker thread.

use std::collections::HashMap;
use std::future::Future;

use anyhow::{anyhow, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use chrono::{DateTime, Utc};
use tokio::runtime::Handle;

use smooth_operator::connector_config::{ConnectorConfig, ConnectorConfigStore};
use smooth_operator::settings::{AgentSettings, SettingsStore};
use smooth_operator_ingestion::indexing::{IndexingRun, IndexingRunStatus, IndexingStore};
use smooth_operator_ingestion::Timestamp;

use crate::checkpoint::aws_err;
use crate::keys::{self, attr};

/// Drive an async future to completion from a *synchronous* trait method.
///
/// Identical bridge to `DynamoCheckpointStore::run_blocking`: `spawn` onto the
/// captured runtime so the async I/O makes progress on that runtime's reactor,
/// then block on the `JoinHandle` from a throwaway OS thread. Never calls
/// `Handle::block_on` on a runtime worker thread.
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

/// Decode a domain struct stored as a JSON string under [`attr::BODY`].
fn body_to<T: serde::de::DeserializeOwned>(item: &HashMap<String, AttributeValue>) -> Result<T> {
    let body = item
        .get(attr::BODY)
        .and_then(|v| v.as_s().ok())
        .ok_or_else(|| anyhow!("item missing '{}' attribute", attr::BODY))?;
    serde_json::from_str(body).map_err(|e| anyhow!("decoding stored body: {e}"))
}

// ---------------------------------------------------------------------------
// Connector config store
// ---------------------------------------------------------------------------

/// DynamoDB-backed [`ConnectorConfigStore`]. Cheap to clone.
#[derive(Clone)]
pub struct DynamoConnectorConfigStore {
    client: Client,
    table: String,
    handle: Handle,
}

impl DynamoConnectorConfigStore {
    /// Build over an existing client + table, capturing the runtime handle.
    #[must_use]
    pub fn new(client: Client, table: impl Into<String>, handle: Handle) -> Self {
        Self {
            client,
            table: table.into(),
            handle,
        }
    }

    async fn list_async(&self, org_id: String) -> Result<Vec<ConnectorConfig>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("#pk = :pk AND begins_with(#sk, :skp)")
            .expression_attribute_names("#pk", attr::PK)
            .expression_attribute_names("#sk", attr::SK)
            .expression_attribute_values(":pk", AttributeValue::S(keys::connector_pk(&org_id)))
            .expression_attribute_values(
                ":skp",
                AttributeValue::S(keys::CONNECTOR_SK_PREFIX.to_string()),
            )
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb list connectors: {}", aws_err(e)))?;
        let mut configs: Vec<ConnectorConfig> =
            out.items().iter().map(body_to).collect::<Result<_>>()?;
        // Match the in-memory store's (name, id) sort.
        configs.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(configs)
    }

    async fn get_async(&self, org_id: String, id: String) -> Result<Option<ConnectorConfig>> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key(attr::PK, AttributeValue::S(keys::connector_pk(&org_id)))
            .key(attr::SK, AttributeValue::S(keys::connector_sk(&id)))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb get connector: {}", aws_err(e)))?;
        match out.item() {
            Some(item) => Ok(Some(body_to(item)?)),
            None => Ok(None),
        }
    }

    async fn upsert_async(&self, cfg: ConnectorConfig) -> Result<()> {
        let body = serde_json::to_string(&cfg)?;
        self.client
            .put_item()
            .table_name(&self.table)
            .item(attr::PK, AttributeValue::S(keys::connector_pk(&cfg.org_id)))
            .item(attr::SK, AttributeValue::S(keys::connector_sk(&cfg.id)))
            .item(
                attr::ENTITY,
                AttributeValue::S("connector-config".to_string()),
            )
            .item(attr::BODY, AttributeValue::S(body))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb upsert connector: {}", aws_err(e)))?;
        Ok(())
    }

    async fn delete_async(&self, org_id: String, id: String) -> Result<bool> {
        // Return the old item so we can report whether a row was actually removed
        // (the trait's `delete` returns bool so the API can 404 an absent id).
        let out = self
            .client
            .delete_item()
            .table_name(&self.table)
            .key(attr::PK, AttributeValue::S(keys::connector_pk(&org_id)))
            .key(attr::SK, AttributeValue::S(keys::connector_sk(&id)))
            .return_values(aws_sdk_dynamodb::types::ReturnValue::AllOld)
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb delete connector: {}", aws_err(e)))?;
        Ok(out.attributes().is_some_and(|a| !a.is_empty()))
    }
}

impl ConnectorConfigStore for DynamoConnectorConfigStore {
    fn list(&self, org_id: &str) -> Vec<ConnectorConfig> {
        let this = self.clone();
        let org = org_id.to_string();
        run_blocking(&self.handle, async move { this.list_async(org).await }).unwrap_or_default()
    }

    fn get(&self, org_id: &str, id: &str) -> Option<ConnectorConfig> {
        let this = self.clone();
        let org = org_id.to_string();
        let id = id.to_string();
        run_blocking(&self.handle, async move { this.get_async(org, id).await })
            .ok()
            .flatten()
    }

    fn upsert(&self, config: ConnectorConfig) {
        let this = self.clone();
        let _ = run_blocking(&self.handle, async move { this.upsert_async(config).await });
    }

    fn delete(&self, org_id: &str, id: &str) -> bool {
        let this = self.clone();
        let org = org_id.to_string();
        let id = id.to_string();
        run_blocking(
            &self.handle,
            async move { this.delete_async(org, id).await },
        )
        .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Settings store
// ---------------------------------------------------------------------------

/// DynamoDB-backed [`SettingsStore`]. Cheap to clone.
#[derive(Clone)]
pub struct DynamoSettingsStore {
    client: Client,
    table: String,
    handle: Handle,
}

impl DynamoSettingsStore {
    /// Build over an existing client + table, capturing the runtime handle.
    #[must_use]
    pub fn new(client: Client, table: impl Into<String>, handle: Handle) -> Self {
        Self {
            client,
            table: table.into(),
            handle,
        }
    }

    async fn get_async(&self, org_id: String) -> Result<Option<AgentSettings>> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key(attr::PK, AttributeValue::S(keys::settings_pk(&org_id)))
            .key(attr::SK, AttributeValue::S(keys::SETTINGS_SK.to_string()))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb get settings: {}", aws_err(e)))?;
        match out.item() {
            Some(item) => Ok(Some(body_to(item)?)),
            None => Ok(None),
        }
    }

    async fn put_async(&self, settings: AgentSettings) -> Result<()> {
        let body = serde_json::to_string(&settings)?;
        self.client
            .put_item()
            .table_name(&self.table)
            .item(
                attr::PK,
                AttributeValue::S(keys::settings_pk(&settings.org_id)),
            )
            .item(attr::SK, AttributeValue::S(keys::SETTINGS_SK.to_string()))
            .item(
                attr::ENTITY,
                AttributeValue::S("agent-settings".to_string()),
            )
            .item(attr::BODY, AttributeValue::S(body))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb put settings: {}", aws_err(e)))?;
        Ok(())
    }
}

impl SettingsStore for DynamoSettingsStore {
    fn get(&self, org_id: &str) -> AgentSettings {
        let this = self.clone();
        let org = org_id.to_string();
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

/// DynamoDB-backed [`IndexingStore`]. Cheap to clone.
#[derive(Clone)]
pub struct DynamoIndexingStore {
    client: Client,
    table: String,
    handle: Handle,
}

impl DynamoIndexingStore {
    /// Build over an existing client + table, capturing the runtime handle.
    #[must_use]
    pub fn new(client: Client, table: impl Into<String>, handle: Handle) -> Self {
        Self {
            client,
            table: table.into(),
            handle,
        }
    }

    async fn record_run_async(&self, run: IndexingRun) -> Result<()> {
        // `IndexingRun` is not (de)serializable (the ingestion crate's contract
        // is intentionally untouched), so the run is stored as discrete
        // attributes — exactly as the Postgres adapter persists it per-column.
        let mut req = self
            .client
            .put_item()
            .table_name(&self.table)
            .item(
                attr::PK,
                AttributeValue::S(keys::indexing_pk(&run.connector_name)),
            )
            .item(
                attr::SK,
                AttributeValue::S(keys::indexing_sk(
                    run.started_at.timestamp_millis(),
                    &run.id,
                )),
            )
            .item(attr::ENTITY, AttributeValue::S("indexing-run".to_string()))
            .item(attr::IX_ID, AttributeValue::S(run.id.clone()))
            .item(
                attr::IX_CONNECTOR,
                AttributeValue::S(run.connector_name.clone()),
            )
            .item(
                attr::IX_STATUS,
                AttributeValue::S(status_to_str(run.status).to_string()),
            )
            .item(
                attr::IX_STARTED_AT,
                AttributeValue::S(run.started_at.to_rfc3339()),
            )
            .item(
                attr::IX_DOCS_SEEN,
                AttributeValue::N(run.documents_seen.to_string()),
            )
            .item(
                attr::IX_CHUNKS,
                AttributeValue::N(run.chunks_indexed.to_string()),
            )
            .item(
                attr::IX_DOCS_SKIPPED,
                AttributeValue::N(run.documents_skipped.to_string()),
            );
        if let Some(finished) = run.finished_at {
            req = req.item(
                attr::IX_FINISHED_AT,
                AttributeValue::S(finished.to_rfc3339()),
            );
        }
        if let Some(cursor) = run.cursor {
            req = req.item(attr::IX_CURSOR, AttributeValue::S(cursor.to_rfc3339()));
        }
        if let Some(error) = &run.error {
            req = req.item(attr::IX_ERROR, AttributeValue::S(error.clone()));
        }
        req.send()
            .await
            .map_err(|e| anyhow!("dynamodb record indexing run: {}", aws_err(e)))?;
        Ok(())
    }

    /// Query a connector's full run partition, oldest-first (ascending SK).
    async fn query_runs(&self, connector_name: &str) -> Result<Vec<IndexingRun>> {
        let mut runs = Vec::new();
        let mut last_key: Option<HashMap<String, AttributeValue>> = None;
        loop {
            let mut req = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("#pk = :pk")
                .expression_attribute_names("#pk", attr::PK)
                .expression_attribute_values(
                    ":pk",
                    AttributeValue::S(keys::indexing_pk(connector_name)),
                )
                .scan_index_forward(true); // oldest-first
            if let Some(start) = last_key.take() {
                req = req.set_exclusive_start_key(Some(start));
            }
            let out = req
                .send()
                .await
                .map_err(|e| anyhow!("dynamodb query indexing runs: {}", aws_err(e)))?;
            for item in out.items() {
                runs.push(item_to_run(item)?);
            }
            match out.last_evaluated_key() {
                Some(k) if !k.is_empty() => last_key = Some(k.clone()),
                _ => break,
            }
        }
        Ok(runs)
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

/// String attribute by name, required.
fn req_s<'a>(item: &'a HashMap<String, AttributeValue>, key: &str) -> Result<&'a str> {
    item.get(key)
        .and_then(|v| v.as_s().ok())
        .map(String::as_str)
        .ok_or_else(|| anyhow!("indexing run item missing string attribute '{key}'"))
}

/// Numeric attribute by name as `usize`, required.
fn req_n_usize(item: &HashMap<String, AttributeValue>, key: &str) -> Result<usize> {
    item.get(key)
        .and_then(|v| v.as_n().ok())
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| anyhow!("indexing run item missing numeric attribute '{key}'"))
}

/// Optional RFC-3339 timestamp attribute by name.
fn opt_ts(item: &HashMap<String, AttributeValue>, key: &str) -> Result<Option<Timestamp>> {
    match item.get(key).and_then(|v| v.as_s().ok()) {
        Some(s) => Ok(Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| anyhow!("parsing '{key}' timestamp: {e}"))?
                .with_timezone(&Utc),
        )),
        None => Ok(None),
    }
}

fn item_to_run(item: &HashMap<String, AttributeValue>) -> Result<IndexingRun> {
    let started_at = opt_ts(item, attr::IX_STARTED_AT)?
        .ok_or_else(|| anyhow!("indexing run item missing 'started_at'"))?;
    Ok(IndexingRun {
        id: req_s(item, attr::IX_ID)?.to_string(),
        connector_name: req_s(item, attr::IX_CONNECTOR)?.to_string(),
        status: status_from_str(req_s(item, attr::IX_STATUS)?)?,
        started_at,
        finished_at: opt_ts(item, attr::IX_FINISHED_AT)?,
        documents_seen: req_n_usize(item, attr::IX_DOCS_SEEN)?,
        chunks_indexed: req_n_usize(item, attr::IX_CHUNKS)?,
        documents_skipped: req_n_usize(item, attr::IX_DOCS_SKIPPED)?,
        cursor: opt_ts(item, attr::IX_CURSOR)?,
        error: item
            .get(attr::IX_ERROR)
            .and_then(|v| v.as_s().ok())
            .cloned(),
    })
}

impl IndexingStore for DynamoIndexingStore {
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
        run_blocking(&self.handle, async move {
            let runs = this.query_runs(&name).await?;
            // Max cursor over SUCCEEDED runs only — a failed run never advances
            // it (mirrors the in-memory store, robust to out-of-order recording).
            Ok(runs
                .iter()
                .filter(|r| r.status == IndexingRunStatus::Succeeded)
                .filter_map(|r| r.cursor)
                .max())
        })
        .ok()
        .flatten()
    }

    fn list_runs(&self, connector_name: &str) -> Vec<IndexingRun> {
        let this = self.clone();
        let name = connector_name.to_string();
        run_blocking(&self.handle, async move { this.query_runs(&name).await }).unwrap_or_default()
    }
}
