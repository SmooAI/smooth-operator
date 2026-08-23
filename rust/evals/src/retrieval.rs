//! Deterministic retrieval-quality eval — recall@k and MRR, no LLM (gap G4).
//!
//! The LLM-judge half of the eval layer ([`crate`] root) needs a live gateway, a
//! key, and money, so it can only run nightly. This half needs **nothing**: it
//! seeds a frozen corpus through the real ingest→chunk→embed→store pipeline,
//! runs a frozen labeled query set through the real
//! [`KnowledgeSearchTool`](smooth_operator::tools::KnowledgeSearchTool), and
//! scores the ranked results with standard IR metrics. Same input, same output,
//! every time — so it runs on **every PR** and catches a retrieval regression
//! (a chunker change, a rerank bug, a store that loses document tails) the day
//! it lands.
//!
//! ## What it does NOT cover
//!
//! The knowledge backend here is `InMemoryKnowledge`, which ranks
//! **lexically**. The [`DeterministicEmbedder`] on the ingest path is really
//! run — batch shape and vector dimension are validated — but its vectors do
//! not influence the ranking, so **an embedder swap is not scored by this
//! eval** (tracked as pearl `th-15a147`). Scoring dense retrieval means running this same corpus and query set
//! against the pgvector adapter under testcontainers; the corpus, labels, and
//! metrics are backend-agnostic and would move over unchanged. Until then, do
//! not read a green run here as "dense retrieval is fine".
//!
//! ## Not gated — deliberately
//!
//! Nothing here reads an env var, a key, or a socket. There is no `#[cfg]`
//! feature, no `SMOOTH_AGENT_E2E` gate, no `#[ignore]`. That is the point: a
//! gated suite that prints `ok. 0 passed` is a suite that did not run, and this
//! repo has been burned by exactly that. If you ever find yourself adding a gate
//! here, you are turning a regression detector back into decoration.
//!
//! ## What is actually under test
//!
//! ```text
//! corpus (frozen)
//!   └─ MockConnector ─▶ ingest() ─▶ Chunker ─▶ DeterministicEmbedder ─▶ InMemoryKnowledge
//!                                                                            │
//!  labeled queries (frozen) ─▶ KnowledgeSearchTool::execute ─────────────────┘
//!                                     │ (optional Reranker)
//!                                     ▼
//!                            ranked results ─▶ recall@k / MRR
//! ```
//!
//! Every box is production code. The eval owns only the corpus, the labels, and
//! the arithmetic.
//!
//! ## Reading the results at the document level
//!
//! Retrieval returns *chunks*; the labels name *documents*. A returned chunk is
//! mapped back to its document by [`KnowledgeResult::source`], which the
//! ingestion pipeline propagates from the source document. Metrics are computed
//! over the ranked chunk list exactly as the model would see it (a document
//! contributing two chunks occupies two ranks), because that ranked list — not
//! some deduplicated ideal — is what lands in the context window.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use smooth_operator::embedding::DeterministicEmbedder;
use smooth_operator::rerank::{LexicalReranker, Reranker};
use smooth_operator::tools::KnowledgeSearchTool;
use smooth_operator_core::{InMemoryKnowledge, KnowledgeBase, Tool};
use smooth_operator_ingestion::{ingest, Chunker, IngestOptions, MockConnector, RawDocument};

use crate::corpus::{corpus, labeled_queries, LabeledQuery};

/// How many results the tool is asked for. Matches the ranked-list depth the
/// metrics are reported at, and stays inside `knowledge_search`'s own 1..=10
/// clamp.
const RETRIEVE_K: usize = 5;

/// A deliberate degradation of the retrieval pipeline.
///
/// These exist so the suite can prove it *detects* a regression rather than
/// merely reporting a number. Each variant breaks one real stage; the suite
/// asserts the metrics fall below the baseline thresholds when it is applied.
/// An eval that has never been shown to fail is theater.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degradation {
    /// No degradation — the shipped configuration.
    None,
    /// Ingest only the first half of the corpus, simulating a connector or
    /// ingest run that silently dropped documents.
    HalfCorpus,
    /// Chunk at 48 chars with no overlap, simulating a chunker regression that
    /// slices facts away from the vocabulary that finds them.
    TinyChunks,
    /// Keep only each document's first paragraph, simulating an extractor or
    /// store write that silently drops everything after the first block — the
    /// failure mode that leaves the corpus looking fully ingested while every
    /// fact stated further down is gone.
    FirstParagraphOnly,
}

/// Which reranker stage the run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankMode {
    /// No reranker — the shipped default (`SMOOTH_AGENT_RERANK` off).
    Off,
    /// The deterministic, network-free [`LexicalReranker`].
    Lexical,
    /// A deliberately broken reranker that reverses the candidate order — what a
    /// flipped comparator in a real reranker looks like. Exists so the suite can
    /// prove it detects a rerank bug rather than merely tolerating one.
    Reversed,
}

/// A reranker with its comparator flipped: worst candidate first.
///
/// Not a fixture standing in for production code — it *is* the bug, injected on
/// purpose so [`Degradation`]-style proof extends to the rerank stage.
struct ReverseReranker;

#[async_trait::async_trait]
impl Reranker for ReverseReranker {
    async fn rerank(
        &self,
        _query: &str,
        mut candidates: Vec<smooth_operator_core::KnowledgeResult>,
        top_k: usize,
    ) -> Vec<smooth_operator_core::KnowledgeResult> {
        candidates.reverse();
        candidates.truncate(top_k);
        candidates
    }
}

/// Configuration for one retrieval-eval run.
#[derive(Debug, Clone, Copy)]
pub struct RetrievalRun {
    /// Degradation to apply (use [`Degradation::None`] for the baseline).
    pub degradation: Degradation,
    /// Reranker stage.
    pub rerank: RerankMode,
}

impl RetrievalRun {
    /// The baseline: shipped configuration, nothing broken.
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            degradation: Degradation::None,
            rerank: RerankMode::Off,
        }
    }

    /// The baseline with a degradation applied.
    #[must_use]
    pub fn degraded(degradation: Degradation) -> Self {
        Self {
            degradation,
            rerank: RerankMode::Off,
        }
    }

    /// This run with the lexical reranker stage enabled.
    #[must_use]
    pub fn with_rerank(mut self) -> Self {
        self.rerank = RerankMode::Lexical;
        self
    }

    /// This run with the deliberately broken (order-reversing) reranker.
    #[must_use]
    pub fn with_broken_rerank(mut self) -> Self {
        self.rerank = RerankMode::Reversed;
        self
    }
}

/// Per-query outcome, kept so a failing run can name the queries that regressed
/// instead of only reporting a mean that moved.
#[derive(Debug, Clone)]
pub struct QueryOutcome {
    /// The query text.
    pub query: &'static str,
    /// The labeled document sources that answer it.
    pub relevant: &'static [&'static str],
    /// Ranked document sources as returned (duplicates kept — a document with
    /// two matching chunks really does occupy two ranks in the model's context).
    pub ranked_sources: Vec<String>,
    /// Fraction of this query's labeled documents present in the top-k.
    pub recall: f32,
    /// Reciprocal of the rank of the first relevant result, or 0.0 if none.
    pub reciprocal_rank: f32,
}

/// Aggregate report for one [`RetrievalRun`].
#[derive(Debug, Clone)]
pub struct RetrievalReport {
    /// The run that produced it.
    pub run: RetrievalRun,
    /// Chunks the ingest pipeline actually stored (proves the corpus landed).
    pub chunks_stored: usize,
    /// Mean recall@3 across the query set.
    pub recall_at_3: f32,
    /// Mean recall@5 across the query set.
    pub recall_at_5: f32,
    /// Mean reciprocal rank across the query set (computed over the top-5 list).
    pub mrr: f32,
    /// Per-query detail.
    pub outcomes: Vec<QueryOutcome>,
}

impl RetrievalReport {
    /// A one-line summary suitable for CI logs.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{:?}/{:?}: chunks={} recall@3={:.3} recall@5={:.3} mrr={:.3}",
            self.run.degradation,
            self.run.rerank,
            self.chunks_stored,
            self.recall_at_3,
            self.recall_at_5,
            self.mrr
        )
    }

    /// Queries where no labeled document made the top-5 — the useful detail when
    /// a threshold assertion fails.
    #[must_use]
    pub fn misses(&self) -> Vec<&QueryOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.reciprocal_rank == 0.0)
            .collect()
    }
}

/// Run the retrieval eval end to end and score it.
///
/// # Errors
/// Propagates ingest and knowledge-query failures — both are bugs in the code
/// under test, not expected conditions, so they surface rather than scoring 0.
pub async fn run_retrieval_eval(run: RetrievalRun) -> Result<RetrievalReport> {
    let knowledge = seed_knowledge(run.degradation).await?;
    let chunks_stored = knowledge.chunks_stored;

    let mut tool = KnowledgeSearchTool::new(Arc::clone(&knowledge.base));
    match run.rerank {
        RerankMode::Off => {}
        RerankMode::Lexical => tool = tool.with_reranker(Arc::new(LexicalReranker::new())),
        RerankMode::Reversed => tool = tool.with_reranker(Arc::new(ReverseReranker)),
    }
    let sink: Arc<Mutex<Vec<smooth_operator_core::KnowledgeResult>>> =
        Arc::new(Mutex::new(Vec::new()));
    let tool = tool.with_result_sink(Arc::clone(&sink));

    let mut outcomes = Vec::new();
    for labeled in labeled_queries() {
        // Drive the real tool, then read the structured results out of the sink
        // rather than re-parsing its prose output.
        tool.execute(serde_json::json!({
            "query": labeled.query,
            "limit": RETRIEVE_K,
        }))
        .await?;

        let ranked_sources: Vec<String> = {
            let mut guard = sink.lock().expect("result sink poisoned");
            guard.drain(..).map(|r| r.source).collect()
        };
        outcomes.push(score_query(labeled, ranked_sources));
    }

    Ok(RetrievalReport {
        run,
        chunks_stored,
        recall_at_3: mean(outcomes.iter().map(|o| recall_at(o, 3))),
        recall_at_5: mean(outcomes.iter().map(|o| o.recall)),
        mrr: mean(outcomes.iter().map(|o| o.reciprocal_rank)),
        outcomes,
    })
}

/// The seeded knowledge base plus how much landed in it.
struct SeededKnowledge {
    base: Arc<dyn KnowledgeBase>,
    chunks_stored: usize,
}

/// Seed the corpus through the real ingestion pipeline, applying `degradation`.
async fn seed_knowledge(degradation: Degradation) -> Result<SeededKnowledge> {
    let mut docs = corpus();
    if degradation == Degradation::HalfCorpus {
        docs.truncate(docs.len() / 2);
    }
    if degradation == Degradation::FirstParagraphOnly {
        docs = docs.into_iter().map(first_paragraph_only).collect();
    }

    let chunker = match degradation {
        // 48 chars with no overlap: small enough that a fact and the words a
        // user would search it by land in different chunks.
        Degradation::TinyChunks => Chunker::new(48, 0),
        _ => Chunker::default(),
    };

    let base: Arc<dyn KnowledgeBase> = Arc::new(InMemoryKnowledge::new());
    let report = ingest(
        &MockConnector::new(docs),
        &chunker,
        &DeterministicEmbedder::new(),
        Arc::clone(&base),
        IngestOptions::for_org("org-northwind"),
    )
    .await?;

    Ok(SeededKnowledge {
        base,
        chunks_stored: report.chunks_stored,
    })
}

/// Keep only a document's first paragraph — the lossy-extraction degradation.
///
/// An earlier version of this degradation truncated every paragraph to its first
/// 40 characters and *improved* the numbers: the in-memory scorer divides the
/// match count by chunk length, so shorter chunks rank higher. That is a real
/// property of the ranker worth knowing, and it is exactly why a degradation has
/// to be measured rather than assumed to hurt.
fn first_paragraph_only(doc: RawDocument) -> RawDocument {
    let content = doc.content.split("\n\n").next().unwrap_or("").to_string();
    RawDocument::new(doc.id, doc.source, content)
}

/// Score one query's ranked source list against its labels.
fn score_query(labeled: &LabeledQuery, ranked_sources: Vec<String>) -> QueryOutcome {
    let relevant: HashSet<&str> = labeled.relevant.iter().copied().collect();

    let hit_count = relevant
        .iter()
        .filter(|label| ranked_sources.iter().any(|s| s == *label))
        .count();
    #[allow(clippy::cast_precision_loss)]
    let recall = hit_count as f32 / relevant.len() as f32;

    let reciprocal_rank = ranked_sources
        .iter()
        .position(|s| relevant.contains(s.as_str()))
        .map_or(0.0, |idx| {
            #[allow(clippy::cast_precision_loss)]
            let rank = (idx + 1) as f32;
            1.0 / rank
        });

    QueryOutcome {
        query: labeled.query,
        relevant: labeled.relevant,
        ranked_sources,
        recall,
        reciprocal_rank,
    }
}

/// Recall recomputed at a shallower cut-off than the retrieved depth.
fn recall_at(outcome: &QueryOutcome, k: usize) -> f32 {
    let top: &[String] = &outcome.ranked_sources[..outcome.ranked_sources.len().min(k)];
    let hits = outcome
        .relevant
        .iter()
        .filter(|label| top.iter().any(|s| s == *label))
        .count();
    #[allow(clippy::cast_precision_loss)]
    let recall = hits as f32 / outcome.relevant.len().max(1) as f32;
    recall
}

/// Arithmetic mean of an iterator of scores; 0.0 for an empty set.
fn mean(values: impl Iterator<Item = f32>) -> f32 {
    let (sum, count) = values.fold((0.0_f32, 0_usize), |(s, c), v| (s + v, c + 1));
    if count == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        let mean = sum / count as f32;
        mean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(ranked: &[&str], relevant: &'static [&'static str]) -> QueryOutcome {
        score_query(
            &LabeledQuery {
                query: "test",
                relevant,
            },
            ranked.iter().map(|s| (*s).to_string()).collect(),
        )
    }

    #[test]
    fn reciprocal_rank_is_one_over_first_relevant_position() {
        assert!((outcome(&["a"], &["a"]).reciprocal_rank - 1.0).abs() < f32::EPSILON);
        assert!((outcome(&["x", "a"], &["a"]).reciprocal_rank - 0.5).abs() < f32::EPSILON);
        assert!((outcome(&["x", "y", "a"], &["a"]).reciprocal_rank - 1.0 / 3.0).abs() < 1e-6);
        assert!(outcome(&["x", "y"], &["a"]).reciprocal_rank == 0.0);
    }

    #[test]
    fn recall_is_fraction_of_labels_retrieved() {
        assert!((outcome(&["a", "b"], &["a", "b"]).recall - 1.0).abs() < f32::EPSILON);
        assert!((outcome(&["a", "x"], &["a", "b"]).recall - 0.5).abs() < f32::EPSILON);
        assert!(outcome(&["x"], &["a", "b"]).recall == 0.0);
    }

    /// A document contributing several chunks occupies several ranks; recall
    /// must still count it once, not once per chunk.
    #[test]
    fn duplicate_chunks_from_one_document_count_once() {
        let o = outcome(&["a", "a", "a"], &["a", "b"]);
        assert!((o.recall - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn mean_of_empty_is_zero() {
        assert!(mean(std::iter::empty()) == 0.0);
    }
}
