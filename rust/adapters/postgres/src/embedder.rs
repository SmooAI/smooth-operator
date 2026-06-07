//! Adapter-specific embedder: the live [`GatewayEmbedder`].
//!
//! The provider-agnostic [`Embedder`] trait, [`InputType`], the network-free
//! [`DeterministicEmbedder`], and [`DEFAULT_EMBEDDING_DIM`] all live in
//! [`smooth_operator_agent_core::embedding`] — the one shared home so the
//! Postgres adapter and the ingestion pipeline embed text identically (same
//! FNV-1a hashing, same L2 normalization, same vectors). This module only holds
//! the adapter-specific [`GatewayEmbedder`]: an OpenAI-compatible
//! `/v1/embeddings` HTTP client (the SmooAI LiteLLM gateway) that drags
//! `reqwest` and lives here rather than in `core`.
//!
//! ## Dimension decision
//!
//! Voyage (`voyage-3-large`, 1024-d) is the production north-star (it backs
//! smooai's `knowledge_vectors`), but Voyage is *not* exposed on the LiteLLM
//! gateway. The gateway does expose OpenAI `text-embedding-3-small` (1536-d).
//! Rather than couple the column width to whichever embedder happens to be
//! configured, the vector dimension is a first-class adapter parameter:
//!
//! | Embedder                | Dim  | Use                              |
//! | ----------------------- | ---- | -------------------------------- |
//! | `DeterministicEmbedder` | 1024 | tests / default (Voyage-shaped)  |
//! | `GatewayEmbedder`       | 1536 | live `text-embedding-3-small`    |
//!
//! The `vector(N)` column and the HNSW index are created at `init` time using
//! the adapter's configured dimension, so dense retrieval is always
//! dimension-consistent.

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use smooth_operator_agent_core::embedding::{Embedder, InputType};

/// Dimension returned by OpenAI `text-embedding-3-small`.
pub const OPENAI_SMALL_EMBEDDING_DIM: usize = 1536;

/// OpenAI-compatible `/v1/embeddings` embedder (the SmooAI LiteLLM gateway).
///
/// Only used when explicitly configured. Reads the endpoint from
/// `SMOOAI_GATEWAY_URL` and the key from `SMOOAI_GATEWAY_KEY` (or pass them in).
/// The default model is `text-embedding-3-small` (1536-d) — set the adapter
/// dimension to [`OPENAI_SMALL_EMBEDDING_DIM`] when using it.
#[derive(Clone)]
pub struct GatewayEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dim: usize,
}

impl GatewayEmbedder {
    /// Build from explicit config.
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        dim: usize,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            dim,
        }
    }

    /// Build from `SMOOAI_GATEWAY_URL` + `SMOOAI_GATEWAY_KEY`, defaulting the
    /// model to `text-embedding-3-small` and the dimension to 1536.
    ///
    /// # Errors
    /// Returns an error if either environment variable is unset.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("SMOOAI_GATEWAY_URL")
            .map_err(|_| anyhow!("SMOOAI_GATEWAY_URL is not set"))?;
        let api_key = std::env::var("SMOOAI_GATEWAY_KEY")
            .map_err(|_| anyhow!("SMOOAI_GATEWAY_KEY is not set"))?;
        Ok(Self::new(
            base_url,
            api_key,
            "text-embedding-3-small",
            OPENAI_SMALL_EMBEDDING_DIM,
        ))
    }
}

#[async_trait]
impl Embedder for GatewayEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed(&self, texts: &[String], _input_type: InputType) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Trim a trailing slash so `{base}/v1/embeddings` is well-formed whether
        // the configured URL ends in `/` or not.
        let url = format!("{}/v1/embeddings", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({ "model": self.model, "input": texts });

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("embeddings request failed ({status}): {text}"));
        }

        #[derive(serde::Deserialize)]
        struct EmbeddingData {
            embedding: Vec<f32>,
            index: usize,
        }
        #[derive(serde::Deserialize)]
        struct EmbeddingResponse {
            data: Vec<EmbeddingData>,
        }

        let mut parsed: EmbeddingResponse = resp.json().await?;
        // OpenAI returns results in request order but documents `index`; sort to
        // be safe, then validate the dimension matches the column.
        parsed.data.sort_by_key(|d| d.index);
        let out: Vec<Vec<f32>> = parsed.data.into_iter().map(|d| d.embedding).collect();

        if out.len() != texts.len() {
            return Err(anyhow!(
                "embeddings count mismatch: got {} for {} inputs",
                out.len(),
                texts.len()
            ));
        }
        for (i, v) in out.iter().enumerate() {
            if v.len() != self.dim {
                return Err(anyhow!(
                    "embedding {i} has dim {} but adapter expects {}",
                    v.len(),
                    self.dim
                ));
            }
        }
        Ok(out)
    }
}
