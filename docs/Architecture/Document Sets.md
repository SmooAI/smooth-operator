# Document sets, curation boosting & query-time metadata filters

Phase 11 ("Document sets / curation / boosting") of the [[Roadmap]].
feature-parity curation features layered onto retrieval **in our service layer** —
exactly like [[Access Control|document-level access control]], and for the same
reason: the engine's `KnowledgeBase::query` returns only
`document_id`/`chunk`/`score`/`source` (not stored metadata), and the in-memory
backend drops document metadata on ingest. So set membership, boost, and metadata
are recorded into a **side table at ingest** and applied at read by over-fetching
and filtering.

Three curation capabilities, all opt-in (default behavior is byte-for-byte
unchanged):

1. **Document sets** — group documents into named sets so a query can be scoped
   ("only the dev-support repo", "only the HR handbook").
2. **Curation boosting** — a numeric per-document multiplier so a curator can
   promote canonical docs (the README, the policy of record) above merely-similar
   matches.
3. **Query-time metadata filters** — restrict retrieval to documents whose stamped
   metadata satisfies key/value equalities ("only prose, not code").

Source: `rust/smooth-operator/src/curation.rs`. Tests:
`rust/smooth-operator/tests/document_sets.rs` (offline, in-memory) + the
`curation` unit module.

---

## Metadata convention

Both fields are plain string entries on `Document.metadata`, stamped **at ingest**
by the ingestion pipeline. A `CuratedKnowledgeStore` parses them into a `DocMeta`
recorded in its side table.

| Key            | Meaning                | Format                                  | Default / malformed         |
| -------------- | ---------------------- | --------------------------------------- | --------------------------- |
| `document_set` | set membership         | **comma-separated** list of set names   | absent ⇒ in no named set    |
| `boost`        | similarity multiplier  | stringified `f32` (e.g. `"3.0"`)         | absent/malformed ⇒ **1.0**  |

- **`document_set` is multi-valued via a comma list**: `"alpha"` is one set;
  `"alpha,beta"` puts the doc in both. Names are trimmed; empty names dropped. A
  multi-set doc appears under a scope to *any* of its sets.
- **`boost`** is parsed to an `f32`. Absent, unparseable (`"abc"`, `""`),
  `NaN`, or non-finite ⇒ `1.0` (a bad stamp can never silently zero out or explode
  a doc's ranking). A negative value is clamped to `0.0` (a curator can bury a doc,
  never invert ordering). `boost > 1.0` promotes; `0.0 ≤ boost < 1.0` demotes.

Builders (`smooth_operator::curation`) stamp these without hand-writing the keys:

```rust
use smooth_operator::curation::{with_boost, with_document_set};

let doc = with_document_set(doc, ["acme/app"]); // tag into a set
let doc = with_boost(doc, 3.0);                 // promote (×3 on the score)
```

The keys are exported as `DocMeta::DOCUMENT_SET_KEY` / `DocMeta::BOOST_KEY`.

---

## `RetrievalFilter`

A query-time filter threaded into retrieval:

```rust
pub struct RetrievalFilter {
    pub document_sets: Option<Vec<String>>,   // None ⇒ unscoped (all docs)
    pub metadata_eq:   HashMap<String, String>, // all (k,v) must match
}
```

- `document_sets: None` ⇒ **no set scoping** (every doc eligible — current/default
  behavior). `Some(["alpha"])` ⇒ only docs in set `alpha`; a doc in **any** listed
  set matches (union). `Some([])` ⇒ matches nothing.
- `metadata_eq` ⇒ every `key == value` must be present and equal in the doc's
  stamped metadata (logical AND). Empty ⇒ no metadata constraint.

Constructors: `RetrievalFilter::none()` (the no-op), `RetrievalFilter::in_sets([…])`,
`.with_metadata_eq(k, v)`. It is `Serialize`/`Deserialize` so a turn's scope can ride
a request payload.

`RetrievalFilter::matches(&DocMeta)` is the predicate; it returns `true` when the
set scope passes (or is `None`) **and** every metadata equality holds.

---

## The `CuratedKnowledgeStore` (over-fetch → filter → boost → re-sort → truncate)

`CuratedKnowledgeStore::new(inner)` wraps any inner `KnowledgeBase`. It mints:

- `ingest_handle()` — an `Arc<dyn KnowledgeBase>` whose `ingest` records each doc's
  `DocMeta` (always) and `DocAcl` (if stamped) into side tables, then forwards the
  doc unchanged to the inner backend. This is the handle the ingestion pipeline
  stores through.
- `reader(filter, access)` — an `Arc<dyn KnowledgeBase>` whose `query`:
  1. **over-fetches** `limit × 5` (floor 20) candidates from the inner backend,
  2. drops anything the requester can't read (**ACL** — see below),
  3. drops anything that fails the `RetrievalFilter` (sets + metadata),
  4. multiplies each survivor's score by its **boost**,
  5. **re-sorts** by the boosted score and **truncates** to `limit`.

A document with no recorded `DocMeta` is treated as an empty one: it is in no named
set (a set-scoped query skips it) and keeps the default `1.0` boost (an unscoped
query still returns it). This makes curation strictly additive — existing/legacy
knowledge stays retrievable with no metadata stamp.

### The boost math

For a candidate with raw similarity `s` and recorded boost `b` (default `1.0`):
`ranked_score = s × b`. Results are re-sorted by `ranked_score` descending
(`f32::total_cmp`, so NaN-safe and total) before truncation. A doc with a *lower*
raw `s` can therefore outrank a higher-`s` doc when its boost is large enough — the
[`boost_reorders_against_raw_similarity`](../../rust/smooth-operator/tests/document_sets.rs)
test pins exactly this (raw `0.40 × 3.0 = 1.2 > 0.667`).

---

## Composition with ACL

A `CuratedKnowledgeStore` records `DocAcl`s at ingest (same `acl_v2` metadata key as
[[Access Control]]) and its `reader` takes an `AccessContext`
alongside the `RetrievalFilter`. **Both filters apply — ACL ∧ set ∧ metadata.** ACL
is evaluated **first**, so a curation filter can only ever *narrow* what a requester
sees, never widen it: a doc in the requested set but restricted to another user is
still dropped. The
[`acl_and_set_filter_both_apply`](../../rust/smooth-operator/tests/document_sets.rs)
test pins this (bob, scoped to set `alpha`, never sees alice's `alpha` doc).

When both `with_access_control(...)` and `with_curation(...)` are set on a runtime,
**curation takes precedence** (it enforces ACL itself, so there's one read pass).

---

## Wiring into a turn

`KnowledgeChatRuntime` and the `knowledge_search` tool both read through the same
filtered, boosted handle:

```rust
let store = CuratedKnowledgeStore::new(storage.knowledge());
// … ingest through store.ingest_handle() …

let runtime = KnowledgeChatRuntime::new(storage, llm)
    .with_curation(
        store,
        AccessContext::for_user("bob"),
        RetrievalFilter::in_sets(["acme/app"]).with_metadata_eq("kind", "prose"),
    );
```

- `with_curation(store, ctx, filter)` scopes the whole turn — both the auto-injected
  `[Relevant knowledge]` context and the agent's `knowledge_search` calls read
  through the curated reader.
- `with_retrieval_filter(filter)` swaps just the filter on an already-configured
  curation store (per-turn scoping without rebuilding the store).
- `KnowledgeSearchTool::with_curation(&store, ctx, filter)` builds the tool directly
  over a curated reader.

Default (neither called) ⇒ retrieval is unchanged.

---

## Ingest-time tagging (how dev-support tags a repo into a set)

`IngestOptions` carries run-level curation so a connector/ingest config tags a whole
source at once:

```rust
ingest(
    &github_connector,
    &Chunker::default(),
    &DeterministicEmbedder::new(),
    curated_store.ingest_handle(),
    IngestOptions::for_org("acme")
        .in_document_sets(["acme/app"]) // tag every chunk into the repo's set
        .with_boost(1.0),               // (optional) promote the whole source
)
.await?;
```

Every stored chunk is stamped `document_set = "acme/app"`, so a later query can be
scoped to just that repo with `RetrievalFilter::in_sets(["acme/app"])`. A chunk's
own propagated `document_set` / `boost` metadata (from the `RawDocument`) takes
precedence over the run-level option, so a connector can set a default set for a
source while still letting a specific document override it — e.g. a connector that
boosts the README above the rest of the repo.

This is the dev-support story: `examples/dev-support` pulls a GitHub repo through the
ingestion pipeline; tagging that run into a set named after the repo lets the
dev-support agent scope answers to a single repo while many repos share one store.

Source: `rust/ingestion/src/pipeline.rs` (`IngestOptions::in_document_sets` /
`with_boost`, applied in `store_chunk`).

---

## Related

- [[Access Control]] — the ACL layer this composes with (same
  over-fetch-then-filter pattern).
- [[Ingestion Pipeline]] — the pull→chunk→embed→store pipeline that stamps the
  curation metadata.
- [[Storage Adapters]] — the `StorageAdapter` knowledge slice the stores wrap.
- [[Roadmap]] — Phase 11.
