//! Text → vector embedding — the shared seam for dense retrieval.
//!
//! Both the Postgres adapter (pgvector knowledge base) and the ingestion
//! pipeline need to turn text into dense vectors, and they must agree byte-for-
//! byte: a document embedded at ingest time and a query embedded at retrieval
//! time only land close together if they went through the *same* projection.
//! This module is that one shared home, so the two consumers can never drift.
//!
//! - [`Embedder`] — the provider-agnostic trait. One vector per input string,
//!   each of length [`Embedder::dim`].
//! - [`DeterministicEmbedder`] — the **default**. A stable hash-based
//!   pseudo-embedding (FNV-1a token hashing, L2-normalized, no network), so
//!   conformance tests are reproducible with zero API calls and zero cost.
//!   Dimension is configurable; the Postgres schema defaults to **1024**
//!   (mirrors smooai's `knowledge_vectors embedding vector(1024)`, Voyage
//!   `voyage-3-large` shape).
//! - [`cosine_similarity`] — a small helper for comparing two vectors (used by
//!   tests and any in-memory ranking that wants to score by dense similarity).
//!
//! Provider-backed embedders live with their consumer: the Postgres adapter's
//! `GatewayEmbedder` (an OpenAI-compatible `/v1/embeddings` HTTP client over the
//! SmooAI LiteLLM gateway) implements this same [`Embedder`] trait but stays in
//! the adapter crate so `core` keeps no heavy HTTP dependency on the dense path.
//!
//! ## Dimension decision
//!
//! Voyage (`voyage-3-large`, 1024-d) is the production north-star (it backs
//! smooai's `knowledge_vectors`), but Voyage is *not* exposed on the LiteLLM
//! gateway. The gateway does expose OpenAI `text-embedding-3-small` (1536-d).
//! Rather than couple the column width to whichever embedder happens to be
//! configured, the vector dimension is a first-class parameter — the Postgres
//! adapter takes its `vector(N)` column width from `embedder.dim()`, so dense
//! retrieval is always dimension-consistent.

use anyhow::Result;
use async_trait::async_trait;

/// Default embedding dimension (Voyage `voyage-3-large` shape; mirrors
/// smooai's `knowledge_vectors embedding vector(1024)`).
pub const DEFAULT_EMBEDDING_DIM: usize = 1024;

/// Whether an embedding is for a document being stored or a search query.
///
/// Voyage and most modern embedding models distinguish the two (asymmetric
/// retrieval). The deterministic embedder ignores it; a provider-backed embedder
/// (e.g. the adapter's `GatewayEmbedder`) maps it onto the request unchanged.
/// The parameter keeps the seam honest for when a Voyage-native gateway lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputType {
    /// Embedding a corpus document for storage.
    Document,
    /// Embedding a user query for retrieval.
    Query,
}

/// Turn text into dense vectors. Implementations must return one vector per
/// input string, each of length [`Embedder::dim`].
#[async_trait]
pub trait Embedder: Send + Sync {
    /// The fixed output dimension. Must equal the `vector(N)` column width.
    fn dim(&self) -> usize;

    /// Embed a batch of texts. Returns `texts.len()` vectors, each `dim()` long.
    ///
    /// # Errors
    /// Returns an error if the backing embedding service fails.
    async fn embed(&self, texts: &[String], input_type: InputType) -> Result<Vec<Vec<f32>>>;
}

/// Deterministic, network-free pseudo-embedder.
///
/// Produces a stable vector from the text via a token-hashing bag-of-words
/// projection, then L2-normalizes it so cosine distance is well-behaved. Same
/// text → same vector, always. This makes pgvector retrieval (and ingestion)
/// tests reproducible without any external service: a document and a query that
/// share salient tokens land close together in the projected space.
#[derive(Debug, Clone)]
pub struct DeterministicEmbedder {
    dim: usize,
}

impl DeterministicEmbedder {
    /// Build with the [`DEFAULT_EMBEDDING_DIM`] (1024).
    #[must_use]
    pub fn new() -> Self {
        Self {
            dim: DEFAULT_EMBEDDING_DIM,
        }
    }

    /// Build with a custom dimension (must match the adapter's `vector(N)`).
    #[must_use]
    pub fn with_dim(dim: usize) -> Self {
        Self { dim }
    }

    /// FNV-1a hash of a token — cheap and stable across runs/platforms.
    fn hash_token(token: &str) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in token.bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// Project one text into a normalized vector of `self.dim` floats.
    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0_f32; self.dim];
        let lower = text.to_lowercase();
        let tokens: Vec<&str> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect();

        for token in tokens {
            let h = Self::hash_token(token);
            // Two hashed buckets per token with deterministic signs spreads the
            // signal so distinct tokens rarely fully collide.
            let idx_a = (h % self.dim as u64) as usize;
            let idx_b = ((h >> 32) % self.dim as u64) as usize;
            let sign_a = if (h & 1) == 0 { 1.0 } else { -1.0 };
            let sign_b = if (h & 2) == 0 { 1.0 } else { -1.0 };
            v[idx_a] += sign_a;
            v[idx_b] += sign_b;
        }

        // L2-normalize so all vectors live on the unit sphere (cosine == dot).
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

impl Default for DeterministicEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Embedder for DeterministicEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed(&self, texts: &[String], _input_type: InputType) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }
}

/// Cosine similarity of two equal-length vectors.
///
/// Returns the dot product over the product of L2 norms, in `[-1.0, 1.0]`. If
/// either vector is zero-length, mismatched in length, or has zero norm, returns
/// `0.0` (orthogonal) rather than `NaN`, so callers can rank without guarding.
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deterministic_is_stable_and_normalized() {
        let e = DeterministicEmbedder::new();
        let a = e
            .embed(&["hello world".to_string()], InputType::Document)
            .await
            .unwrap();
        let b = e
            .embed(&["hello world".to_string()], InputType::Query)
            .await
            .unwrap();
        assert_eq!(a[0].len(), DEFAULT_EMBEDDING_DIM);
        assert_eq!(a, b, "same text must yield the same vector");
        let norm: f32 = a[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "expected unit norm, got {norm}");
    }

    #[tokio::test]
    async fn deterministic_similar_text_is_closer() {
        let e = DeterministicEmbedder::new();
        let vecs = e
            .embed(
                &[
                    "the quick brown fox jumps".to_string(),
                    "the quick brown fox leaps".to_string(),
                    "completely unrelated banana finance report".to_string(),
                ],
                InputType::Document,
            )
            .await
            .unwrap();
        let close = cosine_similarity(&vecs[0], &vecs[1]);
        let far = cosine_similarity(&vecs[0], &vecs[2]);
        assert!(
            close > far,
            "shared-token texts should be more similar ({close} vs {far})"
        );
    }

    #[tokio::test]
    async fn custom_dim_respected() {
        let e = DeterministicEmbedder::with_dim(1536);
        let v = e
            .embed(&["x".to_string()], InputType::Document)
            .await
            .unwrap();
        assert_eq!(v[0].len(), 1536);
    }

    /// Byte-identical-vector guard: a known input must always project to the same
    /// vector prefix. If the hashing/projection ever drifts (across this crate or
    /// a consumer that imports it), this catches it before retrieval changes.
    #[tokio::test]
    async fn known_input_produces_known_vector_prefix() {
        let e = DeterministicEmbedder::with_dim(16);
        let v = e
            .embed(&["return policy refund".to_string()], InputType::Document)
            .await
            .unwrap();
        assert_eq!(v[0].len(), 16);
        // Captured from the FNV-1a token-hash projection (L2-normalized) at the
        // time of consolidation. Any change to the algorithm shifts these.
        let expected: [f32; 16] = [
            -0.28867513,
            0.0,
            0.0,
            -0.28867513,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            -0.28867513,
            0.0,
            -0.86602545,
        ];
        for (i, (got, want)) in v[0].iter().zip(expected.iter()).enumerate() {
            assert!(
                (got - want).abs() < 1e-5,
                "vector drift at index {i}: got {got}, expected {want}"
            );
        }
    }

    #[test]
    fn cosine_similarity_basics() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        // Mismatched length / zero norm → 0.0, never NaN.
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }
}
