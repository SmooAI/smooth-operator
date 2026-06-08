//! Background / incremental indexing (Onyx-gap Phase 11).
//!
//! A connector should re-index on a schedule, pulling only what changed, with
//! per-run status tracking. This module is the *engine* of that loop — the
//! scheduling itself is infrastructure (EventBridge Scheduler → Lambda, or a
//! k8s `CronJob`; see `docs/INDEXING.md`). Each tick calls
//! [`IndexingService::run_once`], which:
//!
//! 1. loads the connector's high-water [`Timestamp`] cursor from an
//!    [`IndexingStore`],
//! 2. pulls only documents newer than that cursor
//!    ([`Connector::pull`]`(since = cursor)`),
//! 3. runs the existing idempotent [`ingest`](crate::ingest) pipeline, and
//! 4. records an [`IndexingRun`] — `Succeeded` with counts + a new cursor (the
//!    max document timestamp seen, or `now` if the source carries none), or
//!    `Failed` with the error and the cursor left un-advanced.
//!
//! ## What makes it incremental + idempotent
//!
//! - **Incremental** is the cursor: `latest_cursor(name)` → `pull(since)`. A
//!   connector that honors `since` (e.g. [`GithubConnector`](crate::GithubConnector)
//!   on the issues API) returns only changed documents; one that can't returns
//!   everything and leans on the ledger below.
//! - **Idempotent** is the [`IngestLedger`](crate::IngestLedger): the pipeline
//!   keys on `(org, doc id, content hash)`, so re-seen unchanged documents store
//!   nothing. See the **ledger caveat** on [`IndexingService`].
//!
//! ## Observability seam
//!
//! [`IndexingService::with_progress`] installs an [`IndexingProgress`] callback
//! the service invokes at start / per stored batch / on finish. A server maps
//! those to the protocol's existing `job_status_updated` events without this
//! crate depending on the protocol. It is a thin seam by design — the full
//! server wiring lives one layer up.

use std::sync::{Arc, Mutex};

use chrono::Utc;

use smooth_operator_core::{DocumentType, KnowledgeBase};

use crate::chunker::Chunker;
use crate::connector::{Connector, Timestamp};
use crate::pipeline::{ingest, IngestLedger, IngestOptions, IngestReport};
use smooth_operator::embedding::Embedder;

/// Terminal (or in-flight) state of an [`IndexingRun`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexingRunStatus {
    /// The run is in flight (set when emitted via the progress hook at start).
    Running,
    /// The run completed; counts and a new cursor are populated.
    Succeeded,
    /// The run errored; [`IndexingRun::error`] holds the message and the cursor
    /// is left un-advanced.
    Failed,
}

/// One record of a single [`IndexingService::run_once`] invocation.
///
/// The row a Postgres/DynamoDB `indexing_runs` table would persist — surfaced
/// live via `job_status_updated` for a connector's indexing status UI.
#[derive(Debug, Clone)]
pub struct IndexingRun {
    /// Unique id for this run (uuid v4).
    pub id: String,
    /// The connector this run indexed (`Connector::name`).
    pub connector_name: String,
    /// Run state.
    pub status: IndexingRunStatus,
    /// When the run started.
    pub started_at: Timestamp,
    /// When the run finished (`None` while `Running`).
    pub finished_at: Option<Timestamp>,
    /// Documents the connector returned this run.
    pub documents_seen: usize,
    /// Chunks newly embedded + stored this run.
    pub chunks_indexed: usize,
    /// Documents skipped as unchanged (`(id, hash)` already in the ledger).
    pub documents_skipped: usize,
    /// New high-water mark — the `since` for the next run. `None` on failure or
    /// when a successful run saw no documents (the prior cursor is preserved in
    /// the store; see [`IndexingService::run_once`]).
    pub cursor: Option<Timestamp>,
    /// Failure message when `status == Failed`.
    pub error: Option<String>,
}

impl IndexingRun {
    /// Whether this run completed successfully.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.status == IndexingRunStatus::Succeeded
    }
}

/// Durable record of indexing runs + per-connector cursors.
///
/// Ships with an [`InMemoryIndexingStore`]. Postgres/DynamoDB stores follow as a
/// sibling table to the existing conversation/checkpoint adapters — `record_run`
/// is an upsert-by-id INSERT, `latest_cursor` is `SELECT max(cursor) WHERE
/// connector_name = $1 AND status = 'Succeeded'`, `list_runs` is an ordered
/// SELECT. Only the trait + in-memory impl are built here; the persistent
/// adapters are intentionally left to the adapter crates.
pub trait IndexingStore: Send + Sync {
    /// Record (insert/upsert) a completed or in-flight run.
    fn record_run(&self, run: &IndexingRun);

    /// The latest successful high-water cursor for `connector_name`, if any —
    /// the `since` to pass to the next pull.
    fn latest_cursor(&self, connector_name: &str) -> Option<Timestamp>;

    /// All runs recorded for `connector_name`, oldest first.
    fn list_runs(&self, connector_name: &str) -> Vec<IndexingRun>;
}

/// In-memory [`IndexingStore`] — runs kept in insertion order; cursor derived
/// from the most recent `Succeeded` run that carried one.
#[derive(Default)]
pub struct InMemoryIndexingStore {
    runs: Mutex<Vec<IndexingRun>>,
}

impl InMemoryIndexingStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl IndexingStore for InMemoryIndexingStore {
    fn record_run(&self, run: &IndexingRun) {
        let mut runs = self.runs.lock().expect("indexing store lock");
        // Upsert by id so a `Running` row can be promoted to a terminal state.
        if let Some(existing) = runs.iter_mut().find(|r| r.id == run.id) {
            *existing = run.clone();
        } else {
            runs.push(run.clone());
        }
    }

    fn latest_cursor(&self, connector_name: &str) -> Option<Timestamp> {
        let runs = self.runs.lock().expect("indexing store lock");
        // Highest cursor among successful runs for this connector — robust to
        // out-of-order recording, and a failed run never advances it.
        runs.iter()
            .filter(|r| {
                r.connector_name == connector_name && r.status == IndexingRunStatus::Succeeded
            })
            .filter_map(|r| r.cursor)
            .max()
    }

    fn list_runs(&self, connector_name: &str) -> Vec<IndexingRun> {
        let runs = self.runs.lock().expect("indexing store lock");
        runs.iter()
            .filter(|r| r.connector_name == connector_name)
            .cloned()
            .collect()
    }
}

/// A lifecycle phase reported to an [`IndexingProgress`] callback.
#[derive(Debug, Clone)]
pub enum IndexingEvent {
    /// The run started; `documents_seen` is the count the connector returned.
    Started {
        connector_name: String,
        documents_seen: usize,
    },
    /// A run finished — terminal snapshot of the [`IndexingRun`].
    Finished(IndexingRun),
}

/// Optional progress callback the service invokes at start / on finish.
///
/// A thin seam: a server implements it to emit the protocol's
/// `job_status_updated` events; tests can capture events in a `Vec`. Kept
/// `Send + Sync` so it can ride on a shared service handle.
pub type IndexingProgress = Arc<dyn Fn(IndexingEvent) + Send + Sync>;

/// Drives a single incremental indexing pass for a connector.
///
/// ## Ledger / idempotency caveat
///
/// `run_once` builds a **fresh** [`IngestLedger`] per call (the in-memory ledger
/// is process-local and not persisted here). With a fresh ledger, a re-seen
/// document re-checks its content hashes; that is *safe* because incrementality
/// comes from the cursor (`pull(since)` doesn't return unchanged docs) and,
/// where a connector can't filter, the knowledge store's id+hash upsert dedups.
/// The **production path** is either to persist the ledger alongside the store
/// (so skips are recorded across processes) or to rely on the store's idempotent
/// upsert — see [`with_ledger`](IndexingService::with_ledger) to inject a shared
/// ledger.
#[derive(Clone)]
pub struct IndexingService {
    org_id: String,
    doc_type: DocumentType,
    ledger: Option<IngestLedger>,
    progress: Option<IndexingProgress>,
}

impl IndexingService {
    /// A service scoped to `org_id` with defaults (`Documentation` docs, a fresh
    /// per-run ledger, no progress hook).
    #[must_use]
    pub fn new(org_id: impl Into<String>) -> Self {
        Self {
            org_id: org_id.into(),
            doc_type: DocumentType::Documentation,
            ledger: None,
            progress: None,
        }
    }

    /// Classify stored documents as `doc_type` (builder).
    #[must_use]
    pub fn doc_type(mut self, doc_type: DocumentType) -> Self {
        self.doc_type = doc_type;
        self
    }

    /// Share a persistent [`IngestLedger`] across runs so unchanged-document
    /// skips are remembered process-to-process (builder). Without this each
    /// `run_once` uses a fresh ledger (see the caveat on [`IndexingService`]).
    #[must_use]
    pub fn with_ledger(mut self, ledger: IngestLedger) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// Install a progress callback (builder) — invoked at start and on finish.
    #[must_use]
    pub fn with_progress(mut self, progress: IndexingProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    fn emit(&self, event: IndexingEvent) {
        if let Some(cb) = &self.progress {
            cb(event);
        }
    }

    /// Run one incremental indexing pass for `connector`.
    ///
    /// Loads the connector's cursor from `store`, pulls only newer documents,
    /// runs the [`ingest`] pipeline, and records an [`IndexingRun`]. The returned
    /// run is the same value recorded in `store`.
    ///
    /// This never returns `Err` for an *indexing* failure — a connector/pipeline
    /// error is captured as a `Failed` [`IndexingRun`] (recorded + returned) so a
    /// scheduler loop observes every tick uniformly. (It can still propagate a
    /// genuinely unexpected panic via the embedder, etc., but the pull/ingest
    /// path is caught.)
    ///
    /// # Errors
    /// Returns `Err` only if recording is impossible; the common
    /// connector/pipeline failures are folded into a `Failed` run instead.
    pub async fn run_once(
        &self,
        connector: &dyn Connector,
        store: &dyn IndexingStore,
        chunker: &Chunker,
        embedder: &dyn Embedder,
        knowledge: Arc<dyn KnowledgeBase>,
    ) -> anyhow::Result<IndexingRun> {
        let connector_name = connector.name().to_string();
        let started_at = Utc::now();
        let id = uuid::Uuid::new_v4().to_string();

        // 1. Incremental cursor: where the last successful run left off.
        let since = store.latest_cursor(&connector_name);

        // Per-run ledger unless a shared one was injected (see caveat).
        let ledger = self.ledger.clone().unwrap_or_default();
        let options = {
            let mut o = IngestOptions::for_org(self.org_id.clone())
                .with_ledger(ledger)
                .doc_type(self.doc_type);
            if let Some(s) = since {
                o = o.since(s);
            }
            o
        };

        // 2. Pull (filtered by `since`) → 3. ingest. We peek the pulled docs to
        // compute the new high-water cursor; the pipeline pulls again internally,
        // which is cheap (the connector is the same handle) and keeps `ingest`'s
        // signature untouched. To avoid a double pull we drive the pipeline and
        // derive the cursor from a single pull here, then ingest those docs.
        let pulled = match connector.pull(since).await {
            Ok(docs) => docs,
            Err(err) => return Ok(self.record_failed(store, id, connector_name, started_at, &err)),
        };
        let documents_seen = pulled.len();
        self.emit(IndexingEvent::Started {
            connector_name: connector_name.clone(),
            documents_seen,
        });

        // High-water mark from the pulled docs' `updated_at` metadata; falls back
        // to `now` when the source carries no timestamps (so a non-incremental
        // connector still advances and won't re-pull the whole world forever —
        // its idempotency then rests on the ledger / store upsert).
        let max_seen = max_updated_at(&pulled);

        // Run the pipeline over exactly the pulled set.
        let report = match ingest_pulled(
            &connector_name,
            pulled,
            chunker,
            embedder,
            Arc::clone(&knowledge),
            options,
        )
        .await
        {
            Ok(report) => report,
            Err(err) => return Ok(self.record_failed(store, id, connector_name, started_at, &err)),
        };

        // 4. New cursor: the max document timestamp seen this run, or — if this
        // run saw documents but none carried a timestamp — `now`. If the run saw
        // NO documents, preserve the prior cursor (don't regress to `now`/`None`).
        let cursor = if documents_seen == 0 {
            since
        } else {
            Some(max_seen.unwrap_or(started_at))
        };

        let run = IndexingRun {
            id,
            connector_name,
            status: IndexingRunStatus::Succeeded,
            started_at,
            finished_at: Some(Utc::now()),
            documents_seen,
            chunks_indexed: report.chunks_stored,
            documents_skipped: report.documents_skipped,
            cursor,
            error: None,
        };
        store.record_run(&run);
        self.emit(IndexingEvent::Finished(run.clone()));
        Ok(run)
    }

    fn record_failed(
        &self,
        store: &dyn IndexingStore,
        id: String,
        connector_name: String,
        started_at: Timestamp,
        err: &anyhow::Error,
    ) -> IndexingRun {
        let run = IndexingRun {
            id,
            connector_name,
            status: IndexingRunStatus::Failed,
            started_at,
            finished_at: Some(Utc::now()),
            documents_seen: 0,
            chunks_indexed: 0,
            documents_skipped: 0,
            cursor: None,
            error: Some(format!("{err:#}")),
        };
        store.record_run(&run);
        self.emit(IndexingEvent::Finished(run.clone()));
        run
    }
}

/// Largest `updated_at` (RFC 3339) across a pulled batch, if any carries one.
fn max_updated_at(docs: &[crate::connector::RawDocument]) -> Option<Timestamp> {
    docs.iter()
        .filter_map(|d| d.metadata.get("updated_at"))
        .filter_map(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .max()
}

/// Ingest an already-pulled batch through chunk → embed → store, reusing the
/// pipeline by wrapping the docs in a fixed-payload connector. Keeps the
/// pipeline's `(id, hash)` idempotency intact while letting `run_once` own the
/// single pull (so the cursor is computed from exactly the indexed set).
async fn ingest_pulled(
    source_name: &str,
    docs: Vec<crate::connector::RawDocument>,
    chunker: &Chunker,
    embedder: &dyn Embedder,
    knowledge: Arc<dyn KnowledgeBase>,
    options: IngestOptions,
) -> anyhow::Result<IngestReport> {
    // A thin replay connector so `ingest` consumes the exact pulled set. `since`
    // is already applied (these docs are post-filter), so it ignores `since`.
    struct Replay {
        name: String,
        docs: Vec<crate::connector::RawDocument>,
    }
    #[async_trait::async_trait]
    impl Connector for Replay {
        fn name(&self) -> &str {
            &self.name
        }
        async fn pull(
            &self,
            _since: Option<Timestamp>,
        ) -> anyhow::Result<Vec<crate::connector::RawDocument>> {
            Ok(self.docs.clone())
        }
    }
    let replay = Replay {
        name: source_name.to_string(),
        docs,
    };
    ingest(&replay, chunker, embedder, knowledge, options).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn run(name: &str, status: IndexingRunStatus, cursor: Option<Timestamp>) -> IndexingRun {
        IndexingRun {
            id: uuid::Uuid::new_v4().to_string(),
            connector_name: name.to_string(),
            status,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            documents_seen: 0,
            chunks_indexed: 0,
            documents_skipped: 0,
            cursor,
            error: None,
        }
    }

    fn ts(y: i32, mo: u32, d: u32) -> Timestamp {
        Utc.with_ymd_and_hms(y, mo, d, 0, 0, 0).unwrap()
    }

    #[test]
    fn store_records_and_lists_in_order() {
        let store = InMemoryIndexingStore::new();
        store.record_run(&run(
            "c",
            IndexingRunStatus::Succeeded,
            Some(ts(2026, 1, 1)),
        ));
        store.record_run(&run(
            "c",
            IndexingRunStatus::Succeeded,
            Some(ts(2026, 1, 2)),
        ));
        store.record_run(&run(
            "other",
            IndexingRunStatus::Succeeded,
            Some(ts(2026, 1, 9)),
        ));

        let runs = store.list_runs("c");
        assert_eq!(runs.len(), 2, "only this connector's runs, insertion order");
        assert_eq!(runs[0].cursor, Some(ts(2026, 1, 1)));
        assert_eq!(runs[1].cursor, Some(ts(2026, 1, 2)));
        assert_eq!(store.list_runs("other").len(), 1);
        assert!(store.list_runs("missing").is_empty());
    }

    #[test]
    fn latest_cursor_is_max_over_successful_runs_only() {
        let store = InMemoryIndexingStore::new();
        store.record_run(&run(
            "c",
            IndexingRunStatus::Succeeded,
            Some(ts(2026, 1, 2)),
        ));
        store.record_run(&run(
            "c",
            IndexingRunStatus::Succeeded,
            Some(ts(2026, 1, 1)),
        ));
        // A later FAILED run with a (nonsense) high cursor must NOT win.
        store.record_run(&run("c", IndexingRunStatus::Failed, Some(ts(2026, 12, 31))));

        assert_eq!(store.latest_cursor("c"), Some(ts(2026, 1, 2)));
        assert_eq!(store.latest_cursor("never-seen"), None);
    }

    #[test]
    fn latest_cursor_none_when_no_successful_cursor() {
        let store = InMemoryIndexingStore::new();
        store.record_run(&run("c", IndexingRunStatus::Failed, None));
        store.record_run(&run("c", IndexingRunStatus::Succeeded, None));
        assert_eq!(store.latest_cursor("c"), None);
    }

    #[test]
    fn record_run_upserts_by_id() {
        let store = InMemoryIndexingStore::new();
        let mut r = run("c", IndexingRunStatus::Running, None);
        store.record_run(&r);
        assert_eq!(store.list_runs("c").len(), 1);
        // Promote the same id to a terminal state — replaces, not appends.
        r.status = IndexingRunStatus::Succeeded;
        r.cursor = Some(ts(2026, 1, 5));
        store.record_run(&r);
        let runs = store.list_runs("c");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, IndexingRunStatus::Succeeded);
        assert_eq!(store.latest_cursor("c"), Some(ts(2026, 1, 5)));
    }
}
