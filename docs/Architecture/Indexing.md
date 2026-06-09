# Background / incremental indexing

The [[Ingestion Pipeline|ingestion crate]] gives you a one-shot pull → chunk → embed →
store pipeline. **Indexing** is what runs that pipeline *on a schedule*, pulling
only what changed since the last run, and recording the status of every run so a
UI can show it. This is the smooth-operator analog of its own background indexing
workers + `index_attempt` table — Phase 11 of the [[Roadmap]].

Lives in `rust/ingestion` (`smooth_operator_ingestion::indexing`):
`IndexingService`, `IndexingStore` (+ `InMemoryIndexingStore`), `IndexingRun`,
and the `IndexingProgress` observability seam.

## The loop

Each scheduled tick calls **`IndexingService::run_once`**:

```text
latest_cursor(name) ─▶ connector.pull(since=cursor) ─▶ ingest(...) ─▶ record_run(IndexingRun)
   (high-water mark)        (only what changed)        (idempotent)      (status + new cursor)
```

1. **Load the cursor.** `store.latest_cursor(connector.name())` returns the
   high-water `Timestamp` of the last *successful* run (or `None` the first
   time).
2. **Pull incrementally.** `connector.pull(Some(cursor))` returns only documents
   changed at/after the cursor. A connector that honors `since` (e.g.
   `GithubConnector` on the issues API, which passes `updated_at` through to the
   GitHub `since` filter) returns just the delta; one that can't returns
   everything and leans on idempotency (below).
3. **Ingest.** The existing `ingest(...)` pipeline chunks, embeds, and stores —
   idempotent on `(org, doc id, content hash)` via the `IngestLedger`.
4. **Record the run.** An `IndexingRun` is written to the `IndexingStore`:
   `Succeeded` with counts (`documents_seen`, `chunks_indexed`,
   `documents_skipped`) and a **new cursor** = the max document `updated_at` seen
   this run (or `now` if the source carries no timestamps); or `Failed` with the
   error message and the cursor **left un-advanced** so the next run retries the
   same window.

`run_once` never returns `Err` for an indexing failure — a connector/pipeline
error is folded into a `Failed` `IndexingRun` (recorded *and* returned) so a
scheduler loop observes every tick uniformly.

## API

```rust
pub struct IndexingRun {
    pub id: String,
    pub connector_name: String,
    pub status: IndexingRunStatus,      // Running | Succeeded | Failed
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub documents_seen: usize,
    pub chunks_indexed: usize,
    pub documents_skipped: usize,
    pub cursor: Option<Timestamp>,      // new high-water mark = next run's `since`
    pub error: Option<String>,
}

pub trait IndexingStore: Send + Sync {
    fn record_run(&self, run: &IndexingRun);
    fn latest_cursor(&self, connector_name: &str) -> Option<Timestamp>;
    fn list_runs(&self, connector_name: &str) -> Vec<IndexingRun>;
}

impl IndexingService {
    pub fn new(org_id: impl Into<String>) -> Self;
    pub fn doc_type(self, doc_type: DocumentType) -> Self;     // builder
    pub fn with_ledger(self, ledger: IngestLedger) -> Self;    // builder
    pub fn with_progress(self, progress: IndexingProgress) -> Self; // builder

    pub async fn run_once(
        &self,
        connector: &dyn Connector,
        store: &dyn IndexingStore,
        chunker: &Chunker,
        embedder: &dyn Embedder,
        knowledge: Arc<dyn KnowledgeBase>,
    ) -> anyhow::Result<IndexingRun>;
}
```

`InMemoryIndexingStore` ships now. **Postgres / DynamoDB stores follow** as a
sibling to the existing conversation/checkpoint adapters — `indexing_runs` is a
plain table:

| column            | notes                                                       |
| ----------------- | ----------------------------------------------------------- |
| `id`              | PK (uuid)                                                   |
| `connector_name`  | indexed                                                     |
| `status`          | `running` \| `succeeded` \| `failed`                        |
| `started_at`      |                                                             |
| `finished_at`     | nullable                                                    |
| `documents_seen`  |                                                             |
| `chunks_indexed`  |                                                             |
| `documents_skipped` |                                                           |
| `cursor`          | nullable timestamp — the high-water mark                    |
| `error`           | nullable text                                               |

- `record_run` → `INSERT … ON CONFLICT (id) DO UPDATE` (upsert, so a `Running`
  row is promoted to terminal).
- `latest_cursor` → `SELECT max(cursor) WHERE connector_name = $1 AND status = 'succeeded'`.
- `list_runs` → ordered `SELECT … WHERE connector_name = $1 ORDER BY started_at`.

Only the trait + in-memory impl are built today; the persistent adapters are
left to the adapter crates.

## Cursor semantics

- The cursor is the **max document `updated_at` seen** this run (read off the
  `RawDocument` `updated_at` metadata key, RFC 3339). It becomes the `since` for
  the next pull.
- A successful run that saw **no documents** preserves the prior cursor (it does
  not regress to `now`/`None`).
- A run whose documents carry **no** `updated_at` advances the cursor to `now`
  (so a non-incremental source doesn't re-pull the whole world forever — its
  idempotency then rests entirely on the ledger / store upsert).
- A **failed** run leaves the cursor un-advanced, so the next run retries the
  same window. `latest_cursor` is computed as the max over *successful* runs, so
  a stray failure can never poison it.

## Ledger / idempotency caveat

`run_once` builds a **fresh `IngestLedger` per call** unless you inject a shared
one via `with_ledger`. With a fresh ledger, a re-seen document re-checks its
content hashes. That is **safe** because:

- incrementality already comes from the cursor (`pull(since)` doesn't return
  unchanged docs), and
- where a connector *can't* filter on `since`, the knowledge store's id+hash
  upsert dedups — re-storing the same `(doc id, content hash)` is a no-op.

The **production path** is one of:

1. **Persist the ledger** alongside the `IndexingStore` (so unchanged-document
   *skips* are remembered process-to-process — useful for accurate
   `documents_skipped` counts), or
2. **Rely on the store's idempotent upsert** (simpler — the cursor handles the
   common case; the upsert handles the rest).

## Observability seam — `IndexingProgress`

`with_progress` installs a callback the service invokes at **start**
(`IndexingEvent::Started { connector_name, documents_seen }`) and on **finish**
(`IndexingEvent::Finished(IndexingRun)`):

```rust
pub type IndexingProgress = Arc<dyn Fn(IndexingEvent) + Send + Sync>;
```

A server maps these to the protocol's existing **`job_status_updated`** events,
so a connector's indexing status streams live to a dashboard — *without* the
ingestion crate depending on the protocol/server. It is a thin seam by design;
the full wiring lives one layer up (the server crate / management UI).

## Scheduling = infrastructure

`run_once` is the engine; **scheduling is deployment**. Two postures, same code
("Smoo-powered or bring-your-own"):

### Serverless (AWS) — the hosted posture

```text
EventBridge Scheduler (rate(1 hour) / cron(...))
        │  one schedule per connector (or one fan-out schedule)
        ▼
   AWS Lambda  ──▶  IndexingService::run_once(connector, store, …)
        │                 │
        │                 ├─ store = PgIndexingStore / DynamoIndexingStore
        │                 └─ knowledge = StorageAdapter knowledge slice
        ▼
   IndexingRun row in `indexing_runs`  ──▶  job_status_updated (live status)
```

- One **EventBridge Scheduler** schedule per configured connector, with a
  `rate(...)` or `cron(...)` expression. The target is a Lambda invoked with the
  connector's config (name + credentials ref).
- The Lambda constructs the connector, the persistent `IndexingStore`, and the
  storage-adapter knowledge slice, then calls `run_once`. Because the cursor
  lives in the store, each invocation is naturally incremental and stateless.
- For long pulls that exceed a single Lambda's budget, wrap the per-connector
  invocation in a **Step Functions** state machine (pull-page → ingest →
  checkpoint cursor → loop) — the cursor makes the pull resumable.

### Kubernetes — the self-host posture

A **`CronJob`** on the same schedule runs a worker container whose entrypoint is
`run_once` over the configured connectors (a `scheduled_index`-style binary).
Same `IndexingStore`, same cursor semantics.

In both postures, the `indexing_runs` rows + `latest_cursor` are what make the
system **incremental** (only pull the delta) and **observable** (every run's
status is recorded and surfaced live).

## Tests

- `rust/ingestion/tests/incremental_indexing.rs` — the headline contract,
  network-free: `run_once` twice against an in-memory knowledge store +
  `InMemoryIndexingStore`. Run 1 pulls all N docs (Succeeded, cursor set); run 2
  (after a newer doc is added) pulls **only** the new one via the run-1 cursor,
  ends with N+1 distinct documents and no duplicates, two runs recorded with
  advancing cursors. Plus the failure path (a `Failed` run, cursor un-advanced)
  and the progress-hook seam (`Started` → `Finished`).
- `IndexingStore` unit tests live in `indexing.rs` (record/latest_cursor/list
  ordering, upsert-by-id, failed-run cursor isolation).

## Related

- [[Ingestion Pipeline]] — the pull → chunk → embed → store pipeline.
- [[Connectors]] — authoring connectors + the GitHub connector.
- [[Roadmap]] — Phase 11.
