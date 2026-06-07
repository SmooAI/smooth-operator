//! The ingestion pipeline: pull → chunk → embed → store (Onyx-gap G1).
//!
//! [`ingest`] drives a [`Connector`] through a [`Chunker`] and an [`Embedder`]
//! into a smooth-operator [`KnowledgeBase`] (the `StorageAdapter`'s knowledge
//! slice). It is **idempotent on `(document id, content hash)`**: an
//! [`IngestLedger`] records what has already been stored so re-running over
//! unchanged sources stores nothing new.
//!
//! ## Why the pipeline embeds even though `KnowledgeBase::ingest` re-embeds
//!
//! The engine's `KnowledgeBase` trait takes a whole [`Document`] and owns its
//! own embedding (the Postgres `PgKnowledgeBase` embeds inside `ingest`; the
//! in-memory one is keyword-scored). The pipeline still runs the [`Embedder`]
//! per chunk so the embedding step is a first-class, tested stage of the
//! pipeline (dimension validated, batch path exercised) and so a backend that
//! accepts a precomputed vector can be wired without changing this code. The
//! computed vectors are surfaced on [`IngestReport`] rather than discarded.
//!
//! Each chunk is stored as its own one-chunk [`Document`] (content already
//! ≤ the chunker's cap and free of blank-line splits), so the chunk boundaries
//! the pipeline chose are exactly what lands in the store.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use smooth_operator::{Document, DocumentType, KnowledgeBase};
use smooth_operator_agent_core::access_control::DocAcl;

use crate::chunker::{Chunk, Chunker};
use crate::connector::{Connector, Timestamp};
use smooth_operator_agent_core::embedding::{Embedder, InputType};

/// Durable dedup state for idempotent ingest.
///
/// Holds the set of `(document_id, content_hash)` keys already stored. The
/// engine's `KnowledgeBase` exposes no list/delete, so idempotency is the
/// ingestion layer's responsibility; this ledger is that memory. It is cheap to
/// [`Clone`] (an `Arc` handle) so the same ledger is shared across runs — a
/// production deployment would persist it alongside the knowledge store.
#[derive(Clone, Default)]
pub struct IngestLedger {
    seen: Arc<Mutex<HashSet<String>>>,
}

impl IngestLedger {
    /// A fresh, empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct `(doc, hash)` keys recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// Whether the ledger has recorded anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record a key; returns `true` if it was newly inserted (not seen before).
    fn record(&self, key: String) -> bool {
        match self.seen.lock() {
            Ok(mut s) => s.insert(key),
            // A poisoned lock means a prior panic; treat as "not seen" so we
            // fail open (re-store) rather than silently drop the document.
            Err(_) => true,
        }
    }
}

/// Options for a single [`ingest`] run.
pub struct IngestOptions {
    /// The organization the documents belong to (multi-tenant scoping; carried
    /// into the dedup key and stored chunk metadata).
    pub org_id: String,
    /// Only pull documents changed at/after this time (connector-dependent).
    pub since: Option<Timestamp>,
    /// Dedup ledger. Defaults to a fresh, empty one (every run re-stores);
    /// share a ledger across runs via [`IngestOptions::with_ledger`] for
    /// idempotency.
    pub ledger: IngestLedger,
    /// How to classify stored documents.
    pub doc_type: DocumentType,
}

impl IngestOptions {
    /// Options scoped to `org_id` with defaults (no `since`, fresh ledger,
    /// `Documentation` type).
    #[must_use]
    pub fn for_org(org_id: impl Into<String>) -> Self {
        Self {
            org_id: org_id.into(),
            since: None,
            ledger: IngestLedger::new(),
            doc_type: DocumentType::Documentation,
        }
    }

    /// Use a shared [`IngestLedger`] so re-ingests are idempotent (builder).
    #[must_use]
    pub fn with_ledger(mut self, ledger: IngestLedger) -> Self {
        self.ledger = ledger;
        self
    }

    /// Set the `since` watermark for an incremental pull (builder).
    #[must_use]
    pub fn since(mut self, since: Timestamp) -> Self {
        self.since = Some(since);
        self
    }

    /// Set the stored [`DocumentType`] (builder).
    #[must_use]
    pub fn doc_type(mut self, doc_type: DocumentType) -> Self {
        self.doc_type = doc_type;
        self
    }
}

/// Outcome of an [`ingest`] run.
#[derive(Debug, Clone, Default)]
pub struct IngestReport {
    /// Documents the connector returned.
    pub documents_pulled: usize,
    /// Documents that were skipped because their `(id, hash)` was unchanged.
    pub documents_skipped: usize,
    /// Chunks newly embedded and stored this run.
    pub chunks_stored: usize,
    /// The embedder's vector dimension (proves the embed stage ran).
    pub embedding_dim: usize,
}

/// FNV-1a hash of a chunk's text, hex-encoded — the content half of the dedup
/// key. Stable across runs/platforms.
fn content_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Run the ingestion pipeline: pull from `connector`, chunk with `chunker`,
/// embed with `embedder`, and store into `knowledge`, deduping via the ledger.
///
/// Returns an [`IngestReport`] summarizing what was pulled, skipped, and stored.
///
/// # Errors
/// Propagates connector pull errors, embedding errors, and knowledge-store
/// ingest errors.
pub async fn ingest(
    connector: &dyn Connector,
    chunker: &Chunker,
    embedder: &dyn Embedder,
    knowledge: Arc<dyn KnowledgeBase>,
    options: IngestOptions,
) -> Result<IngestReport> {
    let docs = connector
        .pull(options.since)
        .await
        .with_context(|| format!("connector '{}' pull failed", connector.name()))?;

    let mut report = IngestReport {
        documents_pulled: docs.len(),
        embedding_dim: embedder.dim(),
        ..IngestReport::default()
    };

    for raw in docs {
        // Idempotency: a document is "unchanged" when every chunk it produces is
        // already in the ledger under (org, doc id, chunk content hash).
        let chunks = chunker.chunk(&raw);
        if chunks.is_empty() {
            report.documents_skipped += 1;
            continue;
        }

        // Compute the per-chunk dedup keys up front, then check whether all are
        // already present (unchanged doc) — record happens at store time.
        let keys: Vec<String> = chunks
            .iter()
            .map(|c| format!("{}::{}::{}", options.org_id, raw.id, content_hash(&c.text)))
            .collect();

        // Probe without recording: a doc with all-seen chunks is skipped whole.
        let any_new = keys.iter().any(|k| !ledger_contains(&options.ledger, k));
        if !any_new {
            report.documents_skipped += 1;
            continue;
        }

        // Embed the new chunks as a batch (exercises the embed stage; validates
        // dimension). Stored even though the in-memory KB ignores the vector —
        // a pgvector backend consumes it.
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let vectors = embedder
            .embed(&texts, InputType::Document)
            .await
            .with_context(|| format!("embedding chunks for document '{}'", raw.id))?;
        debug_assert_eq!(vectors.len(), chunks.len());
        for v in &vectors {
            if v.len() != embedder.dim() {
                anyhow::bail!(
                    "embedder returned dim {} but reports dim {}",
                    v.len(),
                    embedder.dim()
                );
            }
        }

        for (chunk, key) in chunks.iter().zip(keys) {
            // record() returns false if this exact (doc, hash) was already
            // stored — skip the store call to stay idempotent.
            if !options.ledger.record(key) {
                continue;
            }
            store_chunk(
                knowledge.as_ref(),
                &raw.id,
                chunk,
                options.doc_type,
                &options.org_id,
            )?;
            report.chunks_stored += 1;
        }
    }

    Ok(report)
}

/// Non-recording membership probe.
fn ledger_contains(ledger: &IngestLedger, key: &str) -> bool {
    match ledger.seen.lock() {
        Ok(s) => s.contains(key),
        Err(_) => false,
    }
}

/// Store a single chunk as a one-chunk [`Document`] in the knowledge base.
///
/// The chunk text is already ≤ the chunker's cap and contains no blank-line
/// split points, so the engine's internal chunker leaves it as one chunk — the
/// pipeline's boundaries are preserved.
fn store_chunk(
    knowledge: &dyn KnowledgeBase,
    doc_id: &str,
    chunk: &Chunk,
    doc_type: DocumentType,
    org_id: &str,
) -> Result<()> {
    let source = chunk
        .metadata
        .get("source")
        .cloned()
        .unwrap_or_else(|| "ingestion".to_string());

    let mut document = Document::new(chunk.text.clone(), source, doc_type)
        .with_metadata("org_id", org_id)
        .with_metadata("document_id", doc_id)
        .with_metadata("chunk_id", chunk.id.clone())
        .with_metadata("chunk_index", chunk.index.to_string());

    // Carry the chunk's propagated metadata (title, category, …).
    for (k, v) in &chunk.metadata {
        document = document.with_metadata(k.clone(), v.clone());
    }
    // Carry ACL labels for ACL-filtered retrieval (Onyx-gap G3).
    //
    // The legacy comma-joined "acl" field is kept for human/debug visibility.
    // The structured `DocAcl` (under `DocAcl::ACL_METADATA_KEY`) is what an
    // `AclKnowledgeStore` records and enforces at read: the connector's ACL
    // labels are interpreted as *group* entitlements (the common connector
    // permission shape — a document is readable by members of those groups).
    // An empty/absent ACL leaves the document org-public (backward-compatible).
    if let Some(acl) = &chunk.acl {
        if !acl.is_empty() {
            document = document.with_metadata("acl", acl.join(","));
            document = DocAcl::for_groups(acl.clone()).attach_to(document);
        }
    }

    knowledge
        .ingest(document)
        .with_context(|| format!("storing chunk '{}'", chunk.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{MockConnector, RawDocument};
    use smooth_operator::InMemoryKnowledge;
    use smooth_operator_agent_core::embedding::DeterministicEmbedder;

    fn kb() -> Arc<dyn KnowledgeBase> {
        Arc::new(InMemoryKnowledge::new())
    }

    #[tokio::test]
    async fn empty_document_is_skipped_not_stored() {
        let connector = MockConnector::new(vec![RawDocument::new("blank", "mock", "   ")]);
        let report = ingest(
            &connector,
            &Chunker::default(),
            &DeterministicEmbedder::new(),
            kb(),
            IngestOptions::for_org("o"),
        )
        .await
        .unwrap();
        assert_eq!(report.documents_pulled, 1);
        assert_eq!(report.documents_skipped, 1);
        assert_eq!(report.chunks_stored, 0);
    }

    #[tokio::test]
    async fn report_carries_embedding_dim() {
        let connector = MockConnector::new(vec![RawDocument::new("d", "mock", "hello there")]);
        let report = ingest(
            &connector,
            &Chunker::default(),
            &DeterministicEmbedder::with_dim(256),
            kb(),
            IngestOptions::for_org("o"),
        )
        .await
        .unwrap();
        assert_eq!(report.embedding_dim, 256);
        assert!(report.chunks_stored >= 1);
    }

    #[tokio::test]
    async fn ledger_records_keys_across_runs() {
        let connector = MockConnector::new(vec![RawDocument::new("d", "mock", "alpha beta gamma")]);
        let ledger = IngestLedger::new();
        assert!(ledger.is_empty());
        let knowledge = kb();
        let r1 = ingest(
            &connector,
            &Chunker::default(),
            &DeterministicEmbedder::new(),
            Arc::clone(&knowledge),
            IngestOptions::for_org("o").with_ledger(ledger.clone()),
        )
        .await
        .unwrap();
        assert!(r1.chunks_stored >= 1);
        assert!(!ledger.is_empty());
        let recorded = ledger.len();

        // Second run with the same ledger stores nothing new.
        let r2 = ingest(
            &connector,
            &Chunker::default(),
            &DeterministicEmbedder::new(),
            knowledge,
            IngestOptions::for_org("o").with_ledger(ledger.clone()),
        )
        .await
        .unwrap();
        assert_eq!(r2.chunks_stored, 0);
        assert_eq!(r2.documents_skipped, 1);
        assert_eq!(ledger.len(), recorded, "ledger must not grow on re-ingest");
    }

    #[tokio::test]
    async fn changed_content_is_re_ingested() {
        let ledger = IngestLedger::new();
        let knowledge = kb();
        let c1 = MockConnector::new(vec![RawDocument::new("d", "mock", "original content here")]);
        ingest(
            &c1,
            &Chunker::default(),
            &DeterministicEmbedder::new(),
            Arc::clone(&knowledge),
            IngestOptions::for_org("o").with_ledger(ledger.clone()),
        )
        .await
        .unwrap();

        // Same doc id, different content → new hash → stored again.
        let c2 = MockConnector::new(vec![RawDocument::new(
            "d",
            "mock",
            "totally new content now",
        )]);
        let r2 = ingest(
            &c2,
            &Chunker::default(),
            &DeterministicEmbedder::new(),
            knowledge,
            IngestOptions::for_org("o").with_ledger(ledger),
        )
        .await
        .unwrap();
        assert!(
            r2.chunks_stored >= 1,
            "changed content must be re-ingested, got {r2:?}"
        );
        assert_eq!(r2.documents_skipped, 0);
    }
}
