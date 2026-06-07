//! Reranking — the optional post-retrieval reorder stage (Onyx-gap G8).
//!
//! Hybrid retrieval (dense pgvector ∪ sparse `tsvector` BM25 → Reciprocal Rank
//! Fusion) gives a good top-K, but the fusion score is a *rank* signal, not a
//! relevance score against the actual query. A reranker takes that candidate set
//! and reorders it with a sharper query↔candidate relevance model, so the best
//! few float to the top before they reach the model's context window.
//!
//! This is a **seam**, opt-in and behavior-preserving by default:
//!
//! - [`Reranker`] — the trait. `rerank(query, candidates, top_k)` returns a
//!   reordered, truncated `Vec<KnowledgeResult>`.
//! - [`NoopReranker`] — the identity default. Returns the first `top_k`
//!   candidates unchanged, so wiring it in never changes existing behavior.
//! - [`LexicalReranker`] — a deterministic, network-free reranker that scores by
//!   query-term overlap (a small BM25-ish lexical signal). Offline-testable, no
//!   API calls, no cost — the same role the [`DeterministicEmbedder`] plays for
//!   the dense path.
//!
//! ## Where a `GatewayReranker` plugs in
//!
//! A production reranker (Cohere `rerank-3`, Voyage `rerank-2`) is a cross-
//! encoder behind a paid API. It would live in the **adapter** crate (alongside
//! `GatewayEmbedder`), as a `GatewayReranker` that `impl Reranker` and POSTs
//! `{ query, documents, top_n }` to the gateway's `/v1/rerank` endpoint, reading
//! the returned `index → relevance_score` and reordering accordingly. It is
//! deliberately *not* implemented here: `core` keeps no paid-API dependency on
//! the rerank path, exactly as it keeps `GatewayEmbedder` out of the dense path.
//! Swap it in by constructing the runtime / `KnowledgeSearchTool` with
//! `Some(Arc::new(GatewayReranker::from_env()?))` instead of the default.
//!
//! [`DeterministicEmbedder`]: crate::embedding::DeterministicEmbedder

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use smooth_operator::KnowledgeResult;

/// Reorder retrieval candidates by query relevance, returning the top `top_k`.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Rerank `candidates` against `query`, returning at most `top_k`, best
    /// first. Implementations must be total: an empty candidate set yields an
    /// empty result, and `top_k == 0` yields an empty result.
    async fn rerank(
        &self,
        query: &str,
        candidates: Vec<KnowledgeResult>,
        top_k: usize,
    ) -> Vec<KnowledgeResult>;
}

/// Identity reranker — the behavior-preserving default.
///
/// Leaves candidate order untouched and truncates to `top_k`. Wiring this in is
/// a no-op versus not reranking at all, which is exactly what makes the rerank
/// stage opt-in.
#[derive(Debug, Clone, Default)]
pub struct NoopReranker;

#[async_trait]
impl Reranker for NoopReranker {
    async fn rerank(
        &self,
        _query: &str,
        mut candidates: Vec<KnowledgeResult>,
        top_k: usize,
    ) -> Vec<KnowledgeResult> {
        candidates.truncate(top_k);
        candidates
    }
}

/// Deterministic, network-free lexical reranker.
///
/// Scores each candidate by how much of the query's vocabulary its chunk
/// contains — a simple BM25-ish lexical signal (term-frequency saturated and
/// length-normalized) computed entirely offline. No embeddings, no network, no
/// cost, fully reproducible — so it stands in for a paid cross-encoder in tests
/// and as a sane default reorder when no gateway reranker is configured.
///
/// The score per candidate is, over the set of distinct query terms `q` that the
/// candidate contains:
///
/// ```text
/// score = Σ_q  tf_saturated(q) / (1 + ln(1 + chunk_len_in_tokens))
/// ```
///
/// where `tf_saturated(q) = count(q) / (count(q) + K1)` saturates repeated hits
/// so a chunk can't win on raw frequency alone, and the length penalty discounts
/// long chunks that match by sheer size. Ties (and zero-overlap candidates) keep
/// their original relative order (stable sort), so a no-signal query degrades to
/// the upstream ranking rather than shuffling.
#[derive(Debug, Clone)]
pub struct LexicalReranker {
    /// Term-frequency saturation constant (BM25's `k1`-like knob).
    k1: f32,
}

impl LexicalReranker {
    /// Build with a sensible default saturation constant.
    #[must_use]
    pub fn new() -> Self {
        Self { k1: 1.2 }
    }

    /// Build with a custom term-frequency saturation constant.
    #[must_use]
    pub fn with_k1(k1: f32) -> Self {
        Self { k1 }
    }

    /// Lowercase alphanumeric tokenization, matching the dense embedder's split
    /// so the two paths agree on what a "term" is.
    fn tokenize(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Lexical relevance of one candidate chunk against the query terms.
    fn score(&self, query_terms: &HashSet<String>, chunk: &str) -> f32 {
        let chunk_tokens = Self::tokenize(chunk);
        if chunk_tokens.is_empty() {
            return 0.0;
        }
        let length_penalty = 1.0 + (1.0 + chunk_tokens.len() as f32).ln();

        let mut score = 0.0_f32;
        for term in query_terms {
            let count = chunk_tokens.iter().filter(|t| *t == term).count() as f32;
            if count > 0.0 {
                let tf_saturated = count / (count + self.k1);
                score += tf_saturated / length_penalty;
            }
        }
        score
    }
}

impl Default for LexicalReranker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Reranker for LexicalReranker {
    async fn rerank(
        &self,
        query: &str,
        candidates: Vec<KnowledgeResult>,
        top_k: usize,
    ) -> Vec<KnowledgeResult> {
        if candidates.is_empty() || top_k == 0 {
            return Vec::new();
        }
        let query_terms: HashSet<String> = Self::tokenize(query).into_iter().collect();

        // Pair each candidate with its lexical score, then stable-sort by score
        // descending. Stable sort means equal-scored (and zero-overlap)
        // candidates retain their upstream RRF order.
        let mut scored: Vec<(f32, KnowledgeResult)> = candidates
            .into_iter()
            .map(|c| (self.score(&query_terms, &c.chunk), c))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        scored.into_iter().take(top_k).map(|(_, c)| c).collect()
    }
}

/// Apply an optional reranker to a freshly-retrieved candidate set.
///
/// The opt-in retrieval helper: callers pass `Some(reranker)` to reorder the
/// top-K after the knowledge query, or `None` to keep the upstream order
/// (merely truncated to `top_k`). Centralizing the `Option` handling here keeps
/// the call sites (runtime retrieval path, `knowledge_search` tool) minimal and
/// makes "reranking is off by default" a single, obvious branch.
pub async fn apply_optional_rerank(
    reranker: Option<&Arc<dyn Reranker>>,
    query: &str,
    mut candidates: Vec<KnowledgeResult>,
    top_k: usize,
) -> Vec<KnowledgeResult> {
    match reranker {
        Some(r) => r.rerank(query, candidates, top_k).await,
        None => {
            candidates.truncate(top_k);
            candidates
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, chunk: &str, score: f32) -> KnowledgeResult {
        KnowledgeResult {
            document_id: id.to_string(),
            chunk: chunk.to_string(),
            score,
            source: format!("{id}.md"),
        }
    }

    /// TDD: the lexically-best doc is seeded NOT first; the LexicalReranker must
    /// reorder it to the top.
    #[tokio::test]
    async fn lexical_reranker_promotes_best_lexical_match() {
        let query = "return policy refund window";
        // Upstream order puts a weak match first and the strong match third.
        let candidates = vec![
            result(
                "shipping",
                "Standard shipping takes 5 to 7 business days.",
                0.9,
            ),
            result(
                "warranty",
                "Warranty claims must be filed within one year.",
                0.8,
            ),
            result(
                "returns",
                "Our return policy: refunds are issued within the 30 day return window.",
                0.7,
            ),
        ];

        let reranker = LexicalReranker::new();
        let reranked = reranker.rerank(query, candidates, 3).await;

        assert_eq!(
            reranked[0].document_id,
            "returns",
            "the lexically-best doc should be promoted to the top, got order: {:?}",
            reranked
                .iter()
                .map(|r| r.document_id.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// The NoopReranker is the identity (order preserved), just truncating.
    #[tokio::test]
    async fn noop_reranker_is_identity() {
        let query = "anything at all";
        let candidates = vec![
            result("a", "first chunk about returns and refunds", 0.9),
            result("b", "second chunk about shipping", 0.8),
            result("c", "third chunk about returns refund window", 0.7),
        ];
        let original: Vec<String> = candidates.iter().map(|r| r.document_id.clone()).collect();

        let reranked = NoopReranker.rerank(query, candidates, 3).await;
        let after: Vec<String> = reranked.iter().map(|r| r.document_id.clone()).collect();

        assert_eq!(after, original, "noop must preserve order");
    }

    #[tokio::test]
    async fn noop_reranker_truncates_to_top_k() {
        let query = "q";
        let candidates = vec![
            result("a", "alpha", 0.9),
            result("b", "beta", 0.8),
            result("c", "gamma", 0.7),
        ];
        let reranked = NoopReranker.rerank(query, candidates, 2).await;
        assert_eq!(reranked.len(), 2);
        assert_eq!(reranked[0].document_id, "a");
        assert_eq!(reranked[1].document_id, "b");
    }

    #[tokio::test]
    async fn lexical_reranker_truncates_after_reorder() {
        let query = "refund returns";
        let candidates = vec![
            result("shipping", "shipping times and delivery", 0.9),
            result("returns", "refund and returns policy details", 0.8),
            result("misc", "unrelated content here", 0.7),
        ];
        let reranked = LexicalReranker::new().rerank(query, candidates, 1).await;
        assert_eq!(reranked.len(), 1);
        assert_eq!(reranked[0].document_id, "returns");
    }

    #[tokio::test]
    async fn lexical_reranker_no_overlap_preserves_order() {
        // No query term appears in any chunk → all zero scores → stable sort
        // keeps the upstream order intact.
        let query = "quantum entanglement physics";
        let candidates = vec![
            result("a", "shipping and delivery", 0.9),
            result("b", "returns and refunds", 0.8),
        ];
        let reranked = LexicalReranker::new().rerank(query, candidates, 2).await;
        assert_eq!(reranked[0].document_id, "a");
        assert_eq!(reranked[1].document_id, "b");
    }

    #[tokio::test]
    async fn apply_optional_rerank_none_truncates_only() {
        let query = "refund";
        let candidates = vec![
            result("a", "shipping", 0.9),
            result("returns", "refund refund refund window", 0.8),
        ];
        // With None, order is preserved (no reorder), just truncated.
        let out = apply_optional_rerank(None, query, candidates, 2).await;
        assert_eq!(out[0].document_id, "a");
    }

    #[tokio::test]
    async fn apply_optional_rerank_some_reorders() {
        let query = "refund window";
        let candidates = vec![
            result("a", "shipping and delivery times", 0.9),
            result("returns", "refund window details and policy", 0.8),
        ];
        let reranker: Arc<dyn Reranker> = Arc::new(LexicalReranker::new());
        let out = apply_optional_rerank(Some(&reranker), query, candidates, 2).await;
        assert_eq!(out[0].document_id, "returns");
    }
}
