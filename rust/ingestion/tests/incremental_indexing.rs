//! Incremental / background indexing contract (Onyx-gap Phase 11), network-free.
//!
//! The headline test: run [`IndexingService::run_once`] **twice** against an
//! in-memory knowledge store and an [`InMemoryIndexingStore`]. Run 1 pulls every
//! document (no cursor yet) and records a `Succeeded` run with a high-water
//! cursor. Run 2 — after a newer document is added to the connector — receives
//! the run-1 cursor as `since`, so the connector returns **only the new
//! document**; the service indexes just that one, records a second run with an
//! **advanced** cursor, and the knowledge store ends with N+1 distinct documents
//! and zero duplicates.
//!
//! A second test covers the failure path: a connector that errors records a
//! `Failed` run and leaves the cursor un-advanced.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};

use smooth_operator_core::{Document, KnowledgeBase, KnowledgeResult};

use smooth_operator_ingestion::connector::{Connector, RawDocument, Timestamp};
use smooth_operator_ingestion::indexing::{
    InMemoryIndexingStore, IndexingRunStatus, IndexingService, IndexingStore,
};
use smooth_operator_ingestion::{Chunker, DeterministicEmbedder};

/// A connector that carries a per-document timestamp and honors `since`:
/// `pull(None)` returns ALL docs; `pull(Some(t))` returns only docs strictly
/// newer than `t` — exactly the GitHub-issues `since` filter shape.
#[derive(Clone)]
struct TimestampedConnector {
    docs: Arc<Mutex<Vec<(Timestamp, RawDocument)>>>,
}

impl TimestampedConnector {
    fn new() -> Self {
        Self {
            docs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Add a document stamped at `(y, mo, d)` midnight UTC.
    fn add(&self, id: &str, content: &str, y: i32, mo: u32, d: u32) {
        let ts = Utc.with_ymd_and_hms(y, mo, d, 0, 0, 0).unwrap();
        let doc = RawDocument::new(id, "mock", content)
            // The service reads the high-water mark off this metadata key.
            .with_metadata("updated_at", ts.to_rfc3339());
        self.docs.lock().unwrap().push((ts, doc));
    }
}

#[async_trait]
impl Connector for TimestampedConnector {
    fn name(&self) -> &str {
        "timestamped-mock"
    }

    async fn pull(&self, since: Option<Timestamp>) -> Result<Vec<RawDocument>> {
        let docs = self.docs.lock().unwrap();
        Ok(docs
            .iter()
            .filter(|(ts, _)| match since {
                Some(s) => *ts > s,
                None => true,
            })
            .map(|(_, doc)| doc.clone())
            .collect())
    }
}

/// A connector that always errors — drives the failure path.
struct FailingConnector;

#[async_trait]
impl Connector for FailingConnector {
    fn name(&self) -> &str {
        "failing"
    }

    async fn pull(&self, _since: Option<Timestamp>) -> Result<Vec<RawDocument>> {
        anyhow::bail!("source unreachable")
    }
}

/// A `KnowledgeBase` that records the set of distinct source `document_id`s it
/// has stored (the engine's trait exposes no list/count, so the test counts
/// here). Dedup is by the `document_id` metadata the pipeline stamps on each
/// stored chunk.
#[derive(Default)]
struct CountingKnowledge {
    doc_ids: Mutex<HashSet<String>>,
    chunk_count: Mutex<usize>,
}

impl CountingKnowledge {
    fn distinct_documents(&self) -> usize {
        self.doc_ids.lock().unwrap().len()
    }

    fn chunks(&self) -> usize {
        *self.chunk_count.lock().unwrap()
    }
}

impl KnowledgeBase for CountingKnowledge {
    fn ingest(&self, doc: Document) -> Result<()> {
        if let Some(id) = doc.metadata.get("document_id") {
            self.doc_ids.lock().unwrap().insert(id.clone());
        }
        *self.chunk_count.lock().unwrap() += 1;
        Ok(())
    }

    fn query(&self, _query: &str, _limit: usize) -> Result<Vec<KnowledgeResult>> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn run_once_twice_is_incremental_no_duplicates() {
    let connector = TimestampedConnector::new();
    // N = 3 documents, all before the second run's new one.
    connector.add("a", "alpha content one", 2026, 1, 1);
    connector.add("b", "bravo content two", 2026, 1, 2);
    connector.add("c", "charlie content three", 2026, 1, 3);

    let store = InMemoryIndexingStore::new();
    let knowledge: Arc<CountingKnowledge> = Arc::new(CountingKnowledge::default());
    let chunker = Chunker::default();
    let embedder = DeterministicEmbedder::new();

    let service = IndexingService::new("org-acme");

    // --- Run 1: no cursor yet → pull ALL 3, index all, cursor set. ---
    let run1 = service
        .run_once(
            &connector,
            &store,
            &chunker,
            &embedder,
            knowledge.clone() as Arc<dyn KnowledgeBase>,
        )
        .await
        .expect("run 1 ok");

    assert_eq!(run1.status, IndexingRunStatus::Succeeded);
    assert_eq!(run1.documents_seen, 3, "run 1 pulls all N docs");
    assert!(run1.chunks_indexed >= 3);
    assert!(run1.cursor.is_some(), "run 1 sets a cursor");
    assert_eq!(knowledge.distinct_documents(), 3);

    let cursor1 = run1.cursor.unwrap();
    // High-water mark = max updated_at seen = 2026-01-03.
    assert_eq!(cursor1, Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap());
    assert_eq!(store.latest_cursor("timestamped-mock"), Some(cursor1));

    // --- Add one newer doc, then run again. ---
    connector.add("d", "delta content four", 2026, 1, 5);

    let run2 = service
        .run_once(
            &connector,
            &store,
            &chunker,
            &embedder,
            knowledge.clone() as Arc<dyn KnowledgeBase>,
        )
        .await
        .expect("run 2 ok");

    assert_eq!(run2.status, IndexingRunStatus::Succeeded);
    // The connector filtered on the run-1 cursor → only the ONE new doc came back.
    assert_eq!(
        run2.documents_seen, 1,
        "run 2 pulls only the doc newer than the run-1 cursor"
    );
    assert!(run2.chunks_indexed >= 1);

    let cursor2 = run2.cursor.unwrap();
    assert!(cursor2 > cursor1, "cursor advanced: {cursor2} > {cursor1}");
    assert_eq!(cursor2, Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap());

    // Knowledge store ends with N+1 distinct docs, no duplicates.
    assert_eq!(
        knowledge.distinct_documents(),
        4,
        "N+1 distinct documents end-to-end"
    );

    // Two runs recorded, with advancing cursors, both Succeeded.
    let runs = store.list_runs("timestamped-mock");
    assert_eq!(runs.len(), 2, "two IndexingRuns recorded");
    assert!(runs
        .iter()
        .all(|r| r.status == IndexingRunStatus::Succeeded));
    assert!(
        runs[0].cursor.unwrap() < runs[1].cursor.unwrap(),
        "recorded cursors advance run-over-run"
    );

    // Total chunks = run1 + run2 (no re-storing of unchanged docs).
    assert_eq!(
        knowledge.chunks(),
        run1.chunks_indexed + run2.chunks_indexed
    );
}

#[tokio::test]
async fn failure_records_failed_run_and_does_not_advance_cursor() {
    let store = InMemoryIndexingStore::new();
    let knowledge: Arc<CountingKnowledge> = Arc::new(CountingKnowledge::default());
    let chunker = Chunker::default();
    let embedder = DeterministicEmbedder::new();
    let service = IndexingService::new("org-acme");

    let connector = FailingConnector;

    let run = service
        .run_once(
            &connector,
            &store,
            &chunker,
            &embedder,
            knowledge.clone() as Arc<dyn KnowledgeBase>,
        )
        .await
        .expect("run_once itself resolves; the run carries the failure");

    assert_eq!(run.status, IndexingRunStatus::Failed);
    assert!(run.error.is_some(), "error message recorded");
    assert!(run.cursor.is_none(), "failed run carries no cursor");
    assert_eq!(run.documents_seen, 0);
    assert_eq!(knowledge.distinct_documents(), 0, "nothing indexed");

    // Cursor NOT advanced — a later run still pulls from the beginning.
    assert_eq!(
        store.latest_cursor("failing"),
        None,
        "failed run must not advance the high-water cursor"
    );

    // The failed run is still recorded for observability.
    let runs = store.list_runs("failing");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, IndexingRunStatus::Failed);
}

#[tokio::test]
async fn progress_hook_emits_started_then_finished() {
    use smooth_operator_ingestion::indexing::{IndexingEvent, IndexingProgress};

    let connector = TimestampedConnector::new();
    connector.add("a", "alpha content one", 2026, 1, 1);
    connector.add("b", "bravo content two", 2026, 1, 2);

    let store = InMemoryIndexingStore::new();
    let knowledge: Arc<CountingKnowledge> = Arc::new(CountingKnowledge::default());
    let chunker = Chunker::default();
    let embedder = DeterministicEmbedder::new();

    // Capture every emitted event — this is the seam a server maps to the
    // protocol's `job_status_updated` events.
    let events: Arc<Mutex<Vec<IndexingEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let progress: IndexingProgress = Arc::new(move |ev| sink.lock().unwrap().push(ev));

    let service = IndexingService::new("org-acme").with_progress(progress);
    service
        .run_once(
            &connector,
            &store,
            &chunker,
            &embedder,
            knowledge.clone() as Arc<dyn KnowledgeBase>,
        )
        .await
        .expect("run ok");

    let captured = events.lock().unwrap();
    assert_eq!(captured.len(), 2, "Started then Finished");
    match &captured[0] {
        IndexingEvent::Started {
            connector_name,
            documents_seen,
        } => {
            assert_eq!(connector_name, "timestamped-mock");
            assert_eq!(*documents_seen, 2);
        }
        other => panic!("expected Started, got {other:?}"),
    }
    match &captured[1] {
        IndexingEvent::Finished(run) => {
            assert_eq!(run.status, IndexingRunStatus::Succeeded);
            assert_eq!(run.documents_seen, 2);
            assert!(run.cursor.is_some());
        }
        other => panic!("expected Finished, got {other:?}"),
    }
}
