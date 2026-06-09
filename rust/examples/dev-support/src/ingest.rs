//! The `ingest` step: pull the repo through the smooth-operator ingestion
//! pipeline (chunk → embed → store) into a knowledge store the chat runtime
//! reads from.
//!
//! For the demo the store is the in-memory adapter (zero setup); a real
//! deployment would point the pipeline at the Postgres adapter (pgvector) for
//! persistence across restarts — the pipeline + connector code is identical, only
//! the [`StorageAdapter`](smooth_operator::StorageAdapter) changes.

use std::sync::Arc;

use anyhow::{Context, Result};

use smooth_operator::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_ingestion::{
    ingest, Chunker, Connector, GithubConnector, IngestOptions, IngestReport,
};
use smooth_operator_server::embedder::{build_embedder, EmbedderConfig, DEFAULT_EMBEDDING_MODEL};

use crate::config::{DevSupportConfig, DEFAULT_GATEWAY_URL};

/// Build the GitHub [`Connector`] for `config` (resolving `$GITHUB_TOKEN` when
/// `auth = "token"`).
///
/// # Errors
/// Propagates auth resolution failures (e.g. `auth = "token"` with no token).
pub fn build_connector(config: &DevSupportConfig) -> Result<GithubConnector> {
    let connector_config = config
        .connector_config()
        .context("building GitHub connector config")?;
    Ok(GithubConnector::new(connector_config))
}

/// Ingest `connector`'s documents into a fresh in-memory store and return both
/// the populated store and the [`IngestReport`].
///
/// This is the file-free seam the smoke test drives with a `MockConnector`; the
/// `ingest` CLI command calls it with a real [`GithubConnector`].
///
/// # Errors
/// Propagates connector pull, embedding, and store errors.
pub async fn ingest_into_memory(
    connector: &dyn Connector,
    org_id: &str,
) -> Result<(Arc<InMemoryStorageAdapter>, IngestReport)> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let report = ingest_into(
        connector,
        Arc::clone(&storage) as Arc<dyn StorageAdapter>,
        org_id,
    )
    .await?;
    Ok((storage, report))
}

/// Ingest `connector`'s documents into an existing storage adapter's knowledge
/// slice. Lets a caller bring their own (e.g. Postgres) adapter.
///
/// # Errors
/// Propagates connector pull, embedding, and store errors.
pub async fn ingest_into(
    connector: &dyn Connector,
    storage: Arc<dyn StorageAdapter>,
    org_id: &str,
) -> Result<IngestReport> {
    let embedder = build_embedder(&embedder_config_from_env());
    ingest_into_with_embedder(connector, storage, org_id, embedder.as_ref()).await
}

/// As [`ingest_into`] but with a caller-supplied embedder, so the embedder
/// selection is made once by the caller (e.g. `serve` builds it from
/// [`ServerConfig`](smooth_operator_server::config::ServerConfig) and reuses the
/// SAME instance for the Postgres adapter's vector column) — and an offline test
/// can pass the deterministic embedder explicitly, never touching the
/// environment.
///
/// # Errors
/// Propagates connector pull, embedding, and store errors.
pub async fn ingest_into_with_embedder(
    connector: &dyn Connector,
    storage: Arc<dyn StorageAdapter>,
    org_id: &str,
    embedder: &dyn smooth_operator::embedding::Embedder,
) -> Result<IngestReport> {
    ingest(
        connector,
        &Chunker::default(),
        embedder,
        storage.knowledge(),
        IngestOptions::for_org(org_id),
    )
    .await
    .context("running the ingestion pipeline")
}

/// Build the [`EmbedderConfig`] from the same gateway env the chat/serve paths
/// read, so every path selects the **identical** embedder (real semantic
/// `GatewayEmbedder` when `SMOOAI_GATEWAY_KEY` is set, else the network-free
/// deterministic fallback). Centralizing it here means `ingest`, `chat`, and
/// `serve` can never silently disagree on the embedder (and its vector
/// dimension).
#[must_use]
pub fn embedder_config_from_env() -> EmbedderConfig {
    EmbedderConfig {
        gateway_url: std::env::var("SMOOAI_GATEWAY_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string()),
        gateway_key: std::env::var("SMOOAI_GATEWAY_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        model: DEFAULT_EMBEDDING_MODEL.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smooth_operator_ingestion::{MockConnector, RawDocument};

    #[tokio::test]
    async fn ingests_mock_documents_into_memory() {
        let connector = MockConnector::new(vec![
            RawDocument::new(
                "acme/app@main#README.md",
                "https://github.com/acme/app/blob/main/README.md",
                "The frobnicator subsystem uses a 42-slot ring buffer.",
            ),
            RawDocument::new(
                "acme/app@main#src/lib.rs",
                "https://github.com/acme/app/blob/main/src/lib.rs",
                "pub fn frobnicate() { /* ring buffer impl */ }",
            ),
        ]);
        let (storage, report) = ingest_into_memory(&connector, "acme/app").await.unwrap();
        assert_eq!(report.documents_pulled, 2);
        assert!(report.chunks_stored >= 2, "report: {report:?}");

        // The stored knowledge is queryable for the distinctive fact.
        let hits = storage
            .knowledge()
            .query("frobnicator ring buffer", 3)
            .unwrap();
        assert!(
            hits.iter().any(|h| h.chunk.contains("42-slot ring buffer")),
            "expected the README fact to be retrievable, got: {hits:?}"
        );
    }
}
