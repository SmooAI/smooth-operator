//! Embedder selection — the seam that makes production retrieval *real*.
//!
//! Dense retrieval only works if documents and queries are projected by the
//! **same** embedder. There are two implementations of the
//! [`Embedder`](smooth_operator::embedding::Embedder) trait in this workspace:
//!
//! | Embedder                | Dim  | When                                                  |
//! | ----------------------- | ---- | ----------------------------------------------------- |
//! | [`GatewayEmbedder`]     | 1536 | **Production.** `text-embedding-3-small` over the gateway. |
//! | [`DeterministicEmbedder`] | 1024 | Offline / dev / tests. FNV-1a token hash — *not* semantic. |
//!
//! [`build_embedder`] picks between them from configuration: when a gateway key
//! (and URL/model) is present it returns the **real, semantic** [`GatewayEmbedder`];
//! otherwise it falls back to the network-free [`DeterministicEmbedder`] and logs a
//! loud [`tracing::warn!`] so an operator can't mistake a hash-stub index for a
//! real one. The fallback keeps the 257-test offline baseline (and local dev)
//! working with zero credentials.
//!
//! The store dimension **must** match the active embedder's
//! [`dim`](smooth_operator::embedding::Embedder::dim) — mixing 1024-d and 1536-d
//! vectors silently breaks retrieval. Both the server `/index` handler and the
//! `dev-support` example build their embedder here so the choice (and its
//! dimension) is made in exactly one place.

use std::sync::Arc;

use smooth_operator::embedding::{DeterministicEmbedder, Embedder};
use smooth_operator_adapter_postgres::GatewayEmbedder;

/// Inputs the embedder selector needs. A small struct (rather than the whole
/// [`ServerConfig`](crate::config::ServerConfig)) so the `dev-support` example —
/// which has its own config type — can call the same selector.
#[derive(Debug, Clone)]
pub struct EmbedderConfig {
    /// The gateway base URL (e.g. `https://llm.smoo.ai/v1`).
    pub gateway_url: String,
    /// The gateway API key. `Some` ⇒ real [`GatewayEmbedder`]; `None` ⇒ the
    /// deterministic fallback.
    pub gateway_key: Option<String>,
    /// The embedding model id (e.g. `text-embedding-3-small`).
    pub model: String,
}

impl EmbedderConfig {
    /// Build from the server config, defaulting the embedding model.
    #[must_use]
    pub fn from_server_config(config: &crate::config::ServerConfig) -> Self {
        Self {
            gateway_url: config.gateway_url.clone(),
            gateway_key: config.gateway_key.clone(),
            model: DEFAULT_EMBEDDING_MODEL.to_string(),
        }
    }
}

/// The embedding model the gateway selector requests (OpenAI-compatible,
/// 1536-d). Distinct from the *chat* model (`SMOOTH_AGENT_MODEL`).
pub const DEFAULT_EMBEDDING_MODEL: &str = "text-embedding-3-small";

/// Select the embedder for the index/retrieval path from configuration.
///
/// - **Keyed** (`gateway_key` present): the real [`GatewayEmbedder`] —
///   `text-embedding-3-small`, **1536-d**, the production semantic path.
/// - **Unkeyed**: the network-free [`DeterministicEmbedder`] — **1024-d**, a
///   reproducible FNV-1a token hash that is *not* semantic. Logs a loud
///   [`tracing::warn!`] so this can't be mistaken for real retrieval.
///
/// The returned embedder's [`dim`](Embedder::dim) is the source of truth for the
/// store's vector width (1536 vs 1024) — callers must create the knowledge store
/// with `embedder.dim()`, never a hardcoded constant.
#[must_use]
pub fn build_embedder(config: &EmbedderConfig) -> Arc<dyn Embedder> {
    match &config.gateway_key {
        Some(key) if !key.trim().is_empty() => {
            tracing::info!(
                model = %config.model,
                "using GatewayEmbedder (semantic, 1536-d) for retrieval"
            );
            Arc::new(GatewayEmbedder::new(
                config.gateway_url.clone(),
                key.clone(),
                config.model.clone(),
                smooth_operator_adapter_postgres::OPENAI_SMALL_EMBEDDING_DIM,
            ))
        }
        _ => {
            tracing::warn!(
                "using non-semantic DeterministicEmbedder (FNV-1a hash, 1024-d) — \
                 set SMOOAI_GATEWAY_KEY for real semantic retrieval"
            );
            Arc::new(DeterministicEmbedder::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smooth_operator::embedding::DEFAULT_EMBEDDING_DIM;
    use smooth_operator_adapter_postgres::OPENAI_SMALL_EMBEDDING_DIM;

    fn cfg(key: Option<&str>) -> EmbedderConfig {
        EmbedderConfig {
            gateway_url: "https://example.test/v1".into(),
            gateway_key: key.map(str::to_string),
            model: DEFAULT_EMBEDDING_MODEL.to_string(),
        }
    }

    #[test]
    fn keyed_config_selects_gateway_embedder_1536() {
        // A present key ⇒ the real GatewayEmbedder. We assert via its 1536-d
        // signature (no network call — `dim()` is local). This is the production
        // path the adversarial review flagged was never reached.
        let embedder = build_embedder(&cfg(Some("sk-test")));
        assert_eq!(
            embedder.dim(),
            OPENAI_SMALL_EMBEDDING_DIM,
            "keyed config must select the 1536-d GatewayEmbedder"
        );
    }

    #[test]
    fn unkeyed_config_falls_back_to_deterministic_1024() {
        // No key ⇒ the deterministic fallback (the warn! path). 1024-d, offline.
        let embedder = build_embedder(&cfg(None));
        assert_eq!(
            embedder.dim(),
            DEFAULT_EMBEDDING_DIM,
            "unkeyed config must fall back to the 1024-d DeterministicEmbedder"
        );
    }

    #[test]
    fn empty_or_whitespace_key_falls_back_to_deterministic() {
        // A blank/whitespace key is treated as absent (mirrors ServerConfig's
        // own empty-string filtering) — fall back, don't try to auth with "".
        assert_eq!(build_embedder(&cfg(Some(""))).dim(), DEFAULT_EMBEDDING_DIM);
        assert_eq!(
            build_embedder(&cfg(Some("   "))).dim(),
            DEFAULT_EMBEDDING_DIM
        );
    }
}
