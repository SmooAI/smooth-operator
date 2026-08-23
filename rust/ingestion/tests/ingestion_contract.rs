//! Headline acceptance test for the ingestion pipeline (feature gap G1).
//!
//! TDD contract (written before the implementation): build a real in-memory
//! `StorageAdapter` + a `DeterministicEmbedder` + a `MockConnector` carrying a
//! couple of distinctive documents, run `ingest(...)`, and assert the full
//! chunk → embed → store → retrieve round-trip plus idempotency:
//!
//! (a) the connector's documents are chunked and landed in the knowledge slice,
//! (b) a retrieval query for a distinctive term returns the seeded chunk first,
//! (c) re-running `ingest` is idempotent — no duplicate chunks accumulate.
//!
//! No network, no credentials: the connector is a fixture and the embedder is
//! deterministic, so this runs on every PR.

use std::sync::Arc;

use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;

use smooth_operator_ingestion::{
    ingest, Chunker, DeterministicEmbedder, IngestLedger, IngestOptions, MockConnector, RawDocument,
};

/// Two distinctive documents whose salient terms ("zorblax", "quibbleton") do
/// not collide with each other or with ordinary English, so retrieval scoring
/// is unambiguous.
fn fixture_docs() -> Vec<RawDocument> {
    vec![
        RawDocument::new(
            "doc-zorblax",
            "mock",
            "The zorblax is a rare crystalline organism. \
             A zorblax glows faintly under moonlight and feeds on static electricity. \
             Zorblax colonies are found only in the Quibbleton highlands.",
        )
        .with_title("Zorblax Facts")
        .with_metadata("category", "fauna"),
        RawDocument::new(
            "doc-flooble",
            "mock",
            "Flooble engineering is the practice of bending narrow beams. \
             A flooble joint distributes load across three anchor points.",
        )
        .with_title("Flooble Engineering"),
    ]
}

#[tokio::test]
async fn ingest_chunks_embeds_stores_and_retrieves_then_is_idempotent() {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let connector = MockConnector::new(fixture_docs());
    let chunker = Chunker::default();
    let embedder = DeterministicEmbedder::new();
    // The ledger is the durable dedup state. It persists across ingest runs
    // (the engine's KnowledgeBase has no list/delete, so idempotency is the
    // ingestion layer's responsibility). A production backend would back this
    // with the same DB; in-memory here.
    let ledger = IngestLedger::default();

    // ---- first ingest -----------------------------------------------------
    let report = ingest(
        &connector,
        &chunker,
        &embedder,
        storage.knowledge(),
        IngestOptions::for_org("org-acme").with_ledger(ledger.clone()),
    )
    .await
    .expect("first ingest succeeds");

    // (a) Both documents were pulled and produced at least one chunk each.
    assert_eq!(report.documents_pulled, 2, "pulled both fixture docs");
    assert!(
        report.chunks_stored >= 2,
        "expected at least one chunk per doc, got {}",
        report.chunks_stored
    );

    // (b) A distinctive query returns the matching doc's chunk first.
    let kb = storage.knowledge();
    let hits = kb.query("zorblax", 5).expect("query knowledge base");
    assert!(!hits.is_empty(), "zorblax query returned nothing");
    assert!(
        hits[0].chunk.to_lowercase().contains("zorblax"),
        "top hit should be the zorblax chunk, got: {}",
        hits[0].chunk
    );
    // The unrelated flooble doc must not be the top hit for a zorblax query.
    assert!(
        !hits[0].chunk.to_lowercase().contains("flooble"),
        "flooble chunk leaked to the top of a zorblax query"
    );

    // Snapshot how many chunks exist after the first run (count distinct
    // chunks the store will return across a broad query).
    let broad_first = kb.query("zorblax flooble", 100).expect("broad query");
    let count_first = broad_first.len();
    assert!(
        count_first >= 2,
        "expected >=2 stored chunks, got {count_first}"
    );

    // ---- second ingest (idempotency) -------------------------------------
    let report2 = ingest(
        &connector,
        &chunker,
        &embedder,
        storage.knowledge(),
        IngestOptions::for_org("org-acme").with_ledger(ledger.clone()),
    )
    .await
    .expect("second ingest succeeds");

    // Same documents, same content → nothing new should be stored.
    assert_eq!(
        report2.chunks_stored, 0,
        "re-ingesting identical content must store zero new chunks (idempotent)"
    );
    assert_eq!(
        report2.documents_skipped, 2,
        "both unchanged documents should be skipped on re-ingest"
    );

    // (c) The store did not grow.
    let broad_second = kb.query("zorblax flooble", 100).expect("broad query");
    assert_eq!(
        broad_second.len(),
        count_first,
        "re-ingest duplicated chunks: {} before, {} after",
        count_first,
        broad_second.len()
    );
}

/// Three documents whose ACLs differ, for the entitlement half of the contract:
/// one unrestricted (the org-public control), one scoped to `group-eng`, one to
/// `group-fin`. Salient terms are mutually distinct so retrieval is unambiguous.
fn acl_fixture_docs() -> Vec<RawDocument> {
    vec![
        RawDocument::new(
            "doc-open",
            "mock",
            "The grimwald handbook is open to everyone in the company. \
             Grimwald procedures apply to all staff without restriction.",
        )
        .with_title("Grimwald Handbook"),
        RawDocument::new(
            "doc-eng",
            "mock",
            "The snarflex incident postmortem is engineering-only. \
             Snarflex retries were exhausted before the pager fired.",
        )
        .with_title("Snarflex Postmortem")
        .with_acl(vec!["group-eng".to_string()]),
        RawDocument::new(
            "doc-fin",
            "mock",
            "The brindlewick revenue forecast is finance-only. \
             Brindlewick margins are reported quarterly to the board.",
        )
        .with_title("Brindlewick Forecast")
        .with_acl(vec!["group-fin".to_string()]),
    ]
}

/// Access control must survive ingestion — connector-agnostically.
///
/// The end-to-end ACL chain (connector → `DocAcl` → `AclKnowledgeStore` side
/// table → `AclReader`) is also covered in `github_connector.rs`, but only for
/// that one connector. This asserts the same guarantee at the *pipeline* seam,
/// so it holds for every connector and cannot be deleted along with any single
/// one of them. G3 was reopened once already by an ACL layer that existed but
/// was not on the live path; this is the regression fence for the ingest half.
///
/// Every negative assertion below is paired with the positive control that
/// makes it non-vacuous: the same query, run as an entitled principal, must
/// return the document. Otherwise "nothing leaked" would also pass on a
/// pipeline that stored nothing at all.
#[tokio::test]
async fn ingested_acls_gate_retrieval_for_every_connector() {
    use smooth_operator::access_control::{AccessContext, AclKnowledgeStore};

    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    // Wrapping the knowledge slice is what records the ACL at ingest and
    // enforces it at read; the pipeline writes the `DocAcl` this store reads.
    let acl_store = AclKnowledgeStore::new(storage.knowledge());
    let connector = MockConnector::new(acl_fixture_docs());

    let report = ingest(
        &connector,
        &Chunker::default(),
        &DeterministicEmbedder::new(),
        acl_store.ingest_handle(),
        IngestOptions::for_org("org-acme"),
    )
    .await
    .expect("ingest through the ACL store");

    // Positive control on the run itself: an empty run must not be able to
    // satisfy the "no leak" assertions vacuously.
    assert_eq!(report.documents_pulled, 3, "pulled all three fixture docs");
    assert!(
        report.chunks_stored >= 3,
        "expected at least one chunk per doc, got {}",
        report.chunks_stored
    );

    let engineer = acl_store.reader(AccessContext::new(
        Some("alice".into()),
        vec!["group-eng".into()],
    ));
    let financier = acl_store.reader(AccessContext::new(
        Some("bob".into()),
        vec!["group-fin".into()],
    ));
    let anon = acl_store.reader(AccessContext::anonymous());

    // --- the restricted doc is readable by its group (positive control) ------
    let eng_hits = engineer.query("snarflex", 10).expect("engineer query");
    assert!(
        eng_hits
            .iter()
            .any(|h| h.chunk.to_lowercase().contains("snarflex")),
        "group-eng must be able to read the engineering-only doc"
    );

    // --- ...and NOT by a principal outside it (the leak assertion) -----------
    let outsider_hits = financier.query("snarflex", 10).expect("outsider query");
    assert!(
        outsider_hits.is_empty(),
        "group-fin must not read the group-eng doc, got {} hits: {:?}",
        outsider_hits.len(),
        outsider_hits.iter().map(|h| &h.chunk).collect::<Vec<_>>()
    );
    let anon_hits = anon.query("snarflex", 10).expect("anonymous query");
    assert!(
        anon_hits.is_empty(),
        "an anonymous requester must not read a group-restricted doc, got {} hits",
        anon_hits.len()
    );

    // --- symmetric: the finance doc is gated the other way -------------------
    assert!(
        financier
            .query("brindlewick", 10)
            .expect("financier query")
            .iter()
            .any(|h| h.chunk.to_lowercase().contains("brindlewick")),
        "group-fin must be able to read the finance-only doc"
    );
    assert!(
        engineer
            .query("brindlewick", 10)
            .expect("engineer query")
            .is_empty(),
        "group-eng must not read the group-fin doc"
    );

    // --- a doc ingested with no ACL stays org-public -------------------------
    // (Confirms the gate is an opt-in restriction, not a blanket denial that
    // would make the assertions above pass for the wrong reason.)
    for (who, reader) in [("engineer", &engineer), ("financier", &financier)] {
        assert!(
            reader
                .query("grimwald", 10)
                .expect("open-doc query")
                .iter()
                .any(|h| h.chunk.to_lowercase().contains("grimwald")),
            "{who} must still read the unrestricted doc"
        );
    }
    assert!(
        anon.query("grimwald", 10)
            .expect("anon open-doc query")
            .iter()
            .any(|h| h.chunk.to_lowercase().contains("grimwald")),
        "an anonymous requester must still read the unrestricted doc"
    );
}
