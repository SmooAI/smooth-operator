//! Search-quality regression suite — deterministic, network-free, every PR (G4).
//!
//! Seeds a frozen corpus through the real ingest→chunk→embed→store pipeline,
//! runs a frozen labeled query set through the real `knowledge_search` tool, and
//! asserts **recall@k** and **MRR** against fixed thresholds. No LLM, no
//! gateway, no key, no Docker, no clock, no network — so unlike the judged
//! evals it can be a hard CI gate.
//!
//! ## No gate, on purpose
//!
//! There is no `SMOOTH_AGENT_E2E` check, no `#[cfg(feature = …)]`, and no
//! `#[ignore]` anywhere in this file. A gated suite that reports `ok. 0 passed`
//! is a suite that did not run, and this repo has shipped exactly that mistake
//! before. If a future change makes something here need a credential, that
//! something belongs in `llm_judge.rs`, not behind a flag here.
//!
//! ## Thresholds — and the headroom left
//!
//! The thresholds below are **hand-written constants**, never numbers computed
//! from the run. A threshold derived from the code it guards can never fail, and
//! would lock whatever today's ranking does in as "correct" forever.
//!
//! This eval has **zero run-to-run variance** (`eval_is_deterministic_across_runs`
//! proves it), so the headroom is not noise insurance — it is the budget for
//! benign ranking churn. Read it as "how much can degrade before the build goes
//! red":
//!
//! | metric | measured baseline | threshold | headroom |
//! | --- | --- | --- | --- |
//! | recall@3 | 0.975 | 0.90 | ~1.5 of 20 queries may lose their answer |
//! | recall@5 | 1.000 | 0.95 | 1 of 20 queries may lose its answer |
//! | MRR | 0.975 | 0.90 | ~1.5 queries may fall from rank 1 to rank 2 |
//!
//! ## Measured sensitivity (the degradation runs below)
//!
//! | pipeline | recall@3 | recall@5 | MRR | breaches gate |
//! | --- | --- | --- | --- | --- |
//! | shipped config (baseline) | 0.975 | 1.000 | 0.975 | — |
//! | + `LexicalReranker` | 0.975 | 0.975 | 1.000 | — |
//! | half the corpus never ingested | 0.500 | 0.500 | 0.500 | yes |
//! | 48-char chunks, no overlap | 0.975 | 0.975 | 0.842 | yes (MRR) |
//! | first paragraph only | 0.775 | 0.800 | 0.717 | yes |
//! | reranker comparator reversed | 0.325 | 0.450 | 0.250 | yes |
//!
//! ## Running it
//!
//! ```sh
//! cargo test -p smooai-smooth-operator-evals --test retrieval_quality -- --nocapture
//! ```

use smooth_operator_evals::retrieval::{
    run_retrieval_eval, Degradation, RetrievalReport, RetrievalRun,
};

/// Minimum acceptable mean recall@3 for the shipped configuration.
const MIN_RECALL_AT_3: f32 = 0.90;
/// Minimum acceptable mean recall@5 for the shipped configuration.
const MIN_RECALL_AT_5: f32 = 0.95;
/// Minimum acceptable mean reciprocal rank for the shipped configuration.
const MIN_MRR: f32 = 0.90;

/// The corpus must actually land in the store. A pipeline that silently ingests
/// nothing would otherwise score 0 everywhere and read as "retrieval broke"
/// rather than "ingest broke".
const MIN_CHUNKS_STORED: usize = 20;

fn print_report(report: &RetrievalReport) {
    println!("{}", report.summary());
    for outcome in &report.outcomes {
        println!(
            "  rr={:.2} recall={:.2} {:?}\n    → {:?}",
            outcome.reciprocal_rank, outcome.recall, outcome.query, outcome.ranked_sources
        );
    }
}

/// The gate: the shipped retrieval configuration must clear every threshold.
#[tokio::test]
async fn baseline_retrieval_meets_quality_thresholds() {
    let report = run_retrieval_eval(RetrievalRun::baseline())
        .await
        .expect("baseline retrieval eval ran");
    print_report(&report);

    assert!(
        report.chunks_stored >= MIN_CHUNKS_STORED,
        "ingest stored only {} chunks (expected ≥ {MIN_CHUNKS_STORED}) — the corpus did not land, \
         so the quality numbers below are meaningless",
        report.chunks_stored,
    );
    assert!(
        report.recall_at_3 >= MIN_RECALL_AT_3,
        "recall@3 regressed to {:.3} (threshold {MIN_RECALL_AT_3:.2}); queries with no relevant \
         document in the top 5: {:?}",
        report.recall_at_3,
        report.misses().iter().map(|o| o.query).collect::<Vec<_>>(),
    );
    assert!(
        report.recall_at_5 >= MIN_RECALL_AT_5,
        "recall@5 regressed to {:.3} (threshold {MIN_RECALL_AT_5:.2})",
        report.recall_at_5,
    );
    assert!(
        report.mrr >= MIN_MRR,
        "MRR regressed to {:.3} (threshold {MIN_MRR:.2}) — relevant documents are still being \
         found but are ranked lower",
        report.mrr,
    );
}

/// The reranker stage must not make retrieval worse.
///
/// This is the rerank-bug guard: `LexicalReranker` reorders an over-fetched
/// candidate set, and a bug there (a flipped comparator, a bad truncation) shows
/// up as the reranked run scoring below the un-reranked one. Deliberately *not*
/// an assertion that reranking improves the numbers — on this corpus it is
/// roughly neutral, and asserting an improvement that isn't real is how a suite
/// starts lying.
#[tokio::test]
async fn lexical_rerank_does_not_regress_retrieval() {
    let plain = run_retrieval_eval(RetrievalRun::baseline())
        .await
        .expect("baseline ran");
    let reranked = run_retrieval_eval(RetrievalRun::baseline().with_rerank())
        .await
        .expect("reranked run ran");
    println!("{}", plain.summary());
    println!("{}", reranked.summary());

    // A small epsilon absorbs f32 accumulation, not a real ranking change.
    const EPS: f32 = 1e-4;
    assert!(
        reranked.recall_at_3 + EPS >= plain.recall_at_3,
        "rerank stage dropped recall@3 from {:.3} to {:.3}",
        plain.recall_at_3,
        reranked.recall_at_3,
    );
    assert!(
        reranked.mrr + EPS >= plain.mrr,
        "rerank stage dropped MRR from {:.3} to {:.3}",
        plain.mrr,
        reranked.mrr,
    );
    // Deliberately NOT asserted: recall@5. The reranker over-fetches 4×`limit`
    // and truncates back to `limit`, and on this corpus that costs one
    // second-label hit on the multi-document query (1.000 → 0.975) while MRR
    // rises (0.975 → 1.000). That is the ordering-vs-coverage trade-off working
    // as designed, not a regression, so pinning it here would be a false alarm
    // waiting to happen.
}

/// Proof the suite can fail #1: half the corpus never gets ingested.
///
/// An eval that has never been shown to go red is theater. These three tests
/// break one real pipeline stage each and assert the metrics fall through the
/// thresholds the gate above enforces — so the gate's sensitivity is itself
/// under test, permanently, not just on the day it was written.
#[tokio::test]
async fn degradation_half_corpus_breaches_thresholds() {
    let report = run_retrieval_eval(RetrievalRun::degraded(Degradation::HalfCorpus))
        .await
        .expect("degraded run ran");
    print_report(&report);
    assert!(
        report.recall_at_3 < MIN_RECALL_AT_3,
        "dropping half the corpus left recall@3 at {:.3}, still above the {MIN_RECALL_AT_3:.2} \
         threshold — the eval is not sensitive enough to detect lost documents",
        report.recall_at_3,
    );
}

/// Proof the suite can fail #2: a chunker regression that slices facts apart.
#[tokio::test]
async fn degradation_tiny_chunks_breaches_thresholds() {
    let report = run_retrieval_eval(RetrievalRun::degraded(Degradation::TinyChunks))
        .await
        .expect("degraded run ran");
    print_report(&report);
    assert!(
        report.recall_at_3 < MIN_RECALL_AT_3 || report.mrr < MIN_MRR,
        "48-char chunking left recall@3 at {:.3} and MRR at {:.3}, both above threshold — the \
         eval cannot detect a chunker regression",
        report.recall_at_3,
        report.mrr,
    );
}

/// Proof the suite can fail #3: an extractor that drops everything after the
/// first paragraph, so the corpus looks fully ingested but most facts are gone.
#[tokio::test]
async fn degradation_first_paragraph_only_breaches_thresholds() {
    let report = run_retrieval_eval(RetrievalRun::degraded(Degradation::FirstParagraphOnly))
        .await
        .expect("degraded run ran");
    print_report(&report);
    assert!(
        report.recall_at_3 < MIN_RECALL_AT_3,
        "keeping only first paragraphs left recall@3 at {:.3}, still above the \
         {MIN_RECALL_AT_3:.2} threshold — the eval cannot detect a lossy extraction",
        report.recall_at_3,
    );
}

/// Proof the suite can fail #4: the rerank stage itself is buggy.
///
/// `ReverseReranker` is a flipped comparator — the single most likely rerank
/// bug. It leaves ingest, chunking, and the query untouched and only reorders,
/// so MRR is the metric that must catch it. If this ever passes, the suite has
/// stopped watching the rerank stage.
#[tokio::test]
async fn degradation_broken_rerank_breaches_mrr_threshold() {
    let report = run_retrieval_eval(RetrievalRun::baseline().with_broken_rerank())
        .await
        .expect("broken-rerank run ran");
    print_report(&report);
    assert!(
        report.mrr < MIN_MRR,
        "a reranker with its comparator reversed still scored MRR {:.3}, above the \
         {MIN_MRR:.2} threshold — the eval cannot detect a rerank bug",
        report.mrr,
    );
}

/// The eval must be deterministic: two identical runs produce identical numbers.
/// If this ever flakes, every threshold above becomes a coin flip.
#[tokio::test]
async fn eval_is_deterministic_across_runs() {
    let a = run_retrieval_eval(RetrievalRun::baseline())
        .await
        .expect("run a");
    let b = run_retrieval_eval(RetrievalRun::baseline())
        .await
        .expect("run b");
    assert_eq!(a.chunks_stored, b.chunks_stored);
    assert!((a.recall_at_3 - b.recall_at_3).abs() < f32::EPSILON);
    assert!((a.recall_at_5 - b.recall_at_5).abs() < f32::EPSILON);
    assert!((a.mrr - b.mrr).abs() < f32::EPSILON);
    for (x, y) in a.outcomes.iter().zip(&b.outcomes) {
        assert_eq!(x.ranked_sources, y.ranked_sources, "query {:?}", x.query);
    }
}
