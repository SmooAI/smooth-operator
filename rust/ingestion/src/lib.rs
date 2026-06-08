//! # smooth-operator ingestion
//!
//! Knowledge **ingestion + connectors** for smooth-operator — the pipeline
//! that pulls documents from a source, chunks them, embeds them, and stores them
//! in the [`StorageAdapter`](smooth_operator::adapter::StorageAdapter)
//! knowledge slice so they are retrievable. This closes Onyx-gap G1 (knowledge
//! ingestion + connectors), G2 (document chunking pipeline), and G9 (the
//! connector mock + `unit`-vs-`external` test split). See `docs/INGESTION.md`.
//!
//! ## Shape
//!
//! ```text
//! Connector::pull ─▶ Chunker::chunk ─▶ Embedder::embed ─▶ KnowledgeBase::ingest
//!    RawDocument        Vec<Chunk>        Vec<Vec<f32>>        (StorageAdapter
//!                                                               knowledge slice)
//! ```
//!
//! - [`Connector`] — a source of [`RawDocument`]s (`pull(since)`). Built-ins:
//!   [`FileConnector`], [`WebConnector`]; [`MockConnector`] for tests.
//! - [`Chunker`] — paragraph/size split with overlap, stable chunk ids, metadata
//!   propagation (G2).
//! - [`Embedder`] — text→vector seam, shared with the Postgres adapter via
//!   [`smooth_operator::embedding`]; the network-free
//!   [`DeterministicEmbedder`] is the default.
//! - [`ingest`] — the driver, idempotent on `(doc id, content hash)` via an
//!   [`IngestLedger`].
//!
//! ## Wiring example
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use smooth_operator::adapter::StorageAdapter;
//! # use smooth_operator_ingestion::{
//! #     ingest, Chunker, DeterministicEmbedder, FileConnector, IngestOptions,
//! # };
//! # async fn run(storage: Arc<dyn StorageAdapter>) -> anyhow::Result<()> {
//! let connector = FileConnector::new("./docs");
//! let report = ingest(
//!     &connector,
//!     &Chunker::default(),
//!     &DeterministicEmbedder::new(),
//!     storage.knowledge(),
//!     IngestOptions::for_org("org-acme"),
//! )
//! .await?;
//! println!("stored {} chunks", report.chunks_stored);
//! # Ok(())
//! # }
//! ```

pub mod chunker;
pub mod connector;
pub mod connectors;
pub mod indexing;
pub mod pipeline;

pub use chunker::{Chunk, Chunker, DEFAULT_MAX_CHARS, DEFAULT_OVERLAP_CHARS};
pub use connector::{Connector, MockConnector, RawDocument, Timestamp};
pub use connectors::{
    FileConnector, GithubAuth, GithubConnector, GithubConnectorConfig, GithubInclude,
    GithubVisibility, WebConnector,
};
// Background / incremental indexing (Phase 11): the scheduled re-index loop —
// per-connector cursor + per-run status tracking. Scheduling is infrastructure
// (EventBridge Scheduler → Lambda, or k8s CronJob); see `docs/INDEXING.md`.
pub use indexing::{
    InMemoryIndexingStore, IndexingEvent, IndexingProgress, IndexingRun, IndexingRunStatus,
    IndexingService, IndexingStore,
};
// The text→vector seam (trait + deterministic default) lives in core, shared
// with the Postgres adapter so ingestion and retrieval embed identically.
// Re-exported here so existing `ingestion::{Embedder, DeterministicEmbedder, …}`
// consumers keep working.
pub use pipeline::{ingest, IngestLedger, IngestOptions, IngestReport};
pub use smooth_operator::embedding::{
    DeterministicEmbedder, Embedder, InputType, DEFAULT_EMBEDDING_DIM,
};
