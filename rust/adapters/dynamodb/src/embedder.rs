//! Text → vector embedding for the DynamoDB knowledge slice.
//!
//! Both the brute-force DynamoDB retrieval path and the (optional) S3 Vectors
//! path need to turn documents and query strings into dense vectors. We abstract
//! that behind the [`Embedder`] trait so the storage layer never hardcodes a
//! provider — the same seam the Postgres adapter uses.
//!
//! - [`DeterministicEmbedder`] — the **default**. A stable hash-based
//!   pseudo-embedding (no network), L2-normalized, so conformance tests are
//!   reproducible with zero API calls and zero cost. Dimension defaults to
//!   **1024** (Voyage `voyage-3-large` shape, mirroring smooai's production
//!   `knowledge_vectors`).
//!
//! This is a minimal duplication of the Postgres adapter's `Embedder`: each
//! adapter ships its own copy so neither takes a dependency on the other, and
//! the trait surface is small enough that sharing would couple two production
//! backends for no real gain.

use anyhow::Result;
use async_trait::async_trait;

/// Default embedding dimension (Voyage `voyage-3-large` shape; mirrors smooai's
/// `knowledge_vectors embedding vector(1024)`).
pub const DEFAULT_EMBEDDING_DIM: usize = 1024;

/// Whether an embedding is for a document being stored or a search query.
///
/// Voyage and most modern embedding models distinguish the two (asymmetric
/// retrieval). The deterministic embedder ignores it; the parameter keeps the
/// seam honest for when a Voyage-native embedder lands.
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
    /// The fixed output dimension.
    fn dim(&self) -> usize;

    /// Embed a batch of texts. Returns `texts.len()` vectors, each `dim()` long.
    async fn embed(&self, texts: &[String], input_type: InputType) -> Result<Vec<Vec<f32>>>;
}

/// Deterministic, network-free pseudo-embedder.
///
/// Produces a stable vector from the text via a token-hashing bag-of-words
/// projection, then L2-normalizes it so cosine distance is well-behaved. Same
/// text → same vector, always. This makes retrieval tests reproducible without
/// any external service: a document and a query that share salient tokens land
/// close together in the projected space.
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

    /// Build with a custom dimension.
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

/// Cosine similarity of two equal-length vectors. Returns 0.0 if either is
/// zero-length or the lengths differ (defensive — callers embed consistently).
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
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

    #[test]
    fn cosine_handles_degenerate_inputs() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        let v = vec![0.6_f32, 0.8];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }
}
