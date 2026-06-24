//! Reranker selection — the opt-in post-retrieval reorder stage (feature gap G8).
//!
//! Hybrid retrieval (dense ∪ sparse → RRF) gives a good rank-ordered top-K, but
//! the fusion score is a *rank* signal, not a sharp relevance score against the
//! query. A reranker reorders that candidate set with a cross-encoder before it
//! reaches the model. Unlike the embedder — which is *required* for dense
//! retrieval to work at all — the reranker is **opt-in**: the default is the
//! identity [`NoopReranker`], so wiring the selector in never changes existing
//! behavior.
//!
//! There are three implementations of the
//! [`Reranker`](smooth_operator::rerank::Reranker) trait in this workspace:
//!
//! | Reranker            | Network | When                                                       |
//! | ------------------- | ------- | ---------------------------------------------------------- |
//! | [`GatewayReranker`] | yes     | **Production.** Cohere/Voyage `/v1/rerank` over the gateway. |
//! | [`LexicalReranker`] | no      | Offline deterministic reorder (BM25-ish lexical overlap).  |
//! | [`NoopReranker`]    | no      | **Default.** Identity — rerank is off, order unchanged.    |
//!
//! [`build_reranker`] picks from configuration, mirroring
//! [`build_embedder`](crate::embedder::build_embedder):
//!
//! - **Keyed** (gateway key present) ⇒ the real [`GatewayReranker`], the
//!   production semantic reorder. Logs a [`tracing::info!`].
//! - **Unkeyed + lexical requested** (`SMOOTH_AGENT_RERANK=lexical`) ⇒ the
//!   network-free [`LexicalReranker`] for an offline reorder.
//! - **Unkeyed (default)** ⇒ the identity [`NoopReranker`] — rerank is off, so the
//!   271-test baseline (and default behavior) is byte-for-byte unchanged. Logs a
//!   [`tracing::info!`] so an operator can see rerank is disabled.

use std::sync::Arc;

use smooth_operator::rerank::{LexicalReranker, NoopReranker, Reranker};
#[cfg(feature = "postgres")]
use smooth_operator_adapter_postgres::GatewayReranker;

/// The default rerank model. Re-exported from the postgres adapter on the default
/// (cloud) build; defined locally on the lean build so the constant — and any
/// `RerankerConfig` that defaults to it — still resolves without the postgres
/// crate. The two definitions agree.
#[cfg(feature = "postgres")]
pub use smooth_operator_adapter_postgres::DEFAULT_RERANK_MODEL;
#[cfg(not(feature = "postgres"))]
pub const DEFAULT_RERANK_MODEL: &str = "rerank-english-v3.0";

/// Inputs the reranker selector needs. A small struct (rather than the whole
/// [`ServerConfig`](crate::config::ServerConfig)) so other callers can build the
/// same selector. Mirrors [`EmbedderConfig`](crate::embedder::EmbedderConfig).
#[derive(Debug, Clone)]
pub struct RerankerConfig {
    /// The gateway base URL (e.g. `https://llm.smoo.ai/v1`).
    pub gateway_url: String,
    /// The gateway API key. `Some` ⇒ the real [`GatewayReranker`] is eligible.
    pub gateway_key: Option<String>,
    /// The rerank model id (e.g. `rerank-english-v3.0`).
    pub model: String,
    /// Whether the rerank stage is enabled at all. When `false` (the default), the
    /// selector returns the identity [`NoopReranker`] regardless of the key, so
    /// default behavior is unchanged. Driven by `SMOOTH_AGENT_RERANK`.
    pub mode: RerankMode,
}

/// Which rerank stage the operator wants. `Off` is the default so the rerank
/// stage stays opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RerankMode {
    /// Rerank disabled — identity [`NoopReranker`] (default).
    #[default]
    Off,
    /// Gateway cross-encoder if keyed, else fall back to lexical/noop.
    Gateway,
    /// Force the offline deterministic [`LexicalReranker`] (no network).
    Lexical,
}

impl RerankMode {
    /// Parse the `SMOOTH_AGENT_RERANK` env value. Unknown/empty ⇒ [`Off`](Self::Off).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "gateway" | "on" | "1" | "true" => Self::Gateway,
            "lexical" => Self::Lexical,
            _ => Self::Off,
        }
    }
}

impl RerankerConfig {
    /// Read the rerank mode from `SMOOTH_AGENT_RERANK` (unset ⇒ [`Off`](RerankMode::Off)).
    #[must_use]
    pub fn mode_from_env() -> RerankMode {
        std::env::var("SMOOTH_AGENT_RERANK")
            .ok()
            .map(|s| RerankMode::parse(&s))
            .unwrap_or_default()
    }

    /// Build from the gateway parts + `SMOOTH_AGENT_RERANK`, defaulting the rerank
    /// model. The shared constructor so both the reference server's `ServerConfig`
    /// and the lambda's `LambdaConfig` select rerank identically.
    #[must_use]
    pub fn from_gateway(gateway_url: impl Into<String>, gateway_key: Option<String>) -> Self {
        Self {
            gateway_url: gateway_url.into(),
            gateway_key,
            model: DEFAULT_RERANK_MODEL.to_string(),
            mode: Self::mode_from_env(),
        }
    }

    /// Build from the server config + `SMOOTH_AGENT_RERANK`, defaulting the rerank
    /// model.
    #[must_use]
    pub fn from_server_config(config: &crate::config::ServerConfig) -> Self {
        Self::from_gateway(config.gateway_url.clone(), config.gateway_key.clone())
    }
}

/// Select the reranker for the retrieval path from configuration.
///
/// Returns `None` when rerank is disabled (the default), which the retrieval path
/// treats as "don't reorder" — keeping default behavior byte-for-byte unchanged.
/// Returns `Some(reranker)` only when explicitly enabled via `SMOOTH_AGENT_RERANK`:
///
/// - `gateway` + a gateway key ⇒ the real [`GatewayReranker`] (production).
/// - `gateway` without a key, or `lexical` ⇒ the offline [`LexicalReranker`].
/// - `off` / unset ⇒ `None` (no rerank).
#[must_use]
pub fn build_reranker(config: &RerankerConfig) -> Option<Arc<dyn Reranker>> {
    match config.mode {
        RerankMode::Off => {
            tracing::info!("rerank stage disabled (default) — retrieval order unchanged");
            None
        }
        RerankMode::Gateway => match &config.gateway_key {
            // The real GatewayReranker lives in the postgres adapter crate, so
            // it's only available on a build with the `postgres` feature (the
            // default / cloud build). On a lean `--no-default-features` build this
            // arm is compiled out and gateway mode falls back to the offline
            // LexicalReranker below regardless of the key.
            #[cfg(feature = "postgres")]
            Some(key) if !key.trim().is_empty() => {
                tracing::info!(
                    model = %config.model,
                    "using GatewayReranker (cross-encoder /v1/rerank) for retrieval reorder"
                );
                Some(Arc::new(GatewayReranker::new(
                    config.gateway_url.clone(),
                    key.clone(),
                    config.model.clone(),
                )))
            }
            _ => {
                tracing::warn!(
                    "SMOOTH_AGENT_RERANK=gateway but no GatewayReranker available \
                     (no gateway key, or a lean build without the `postgres` feature) — \
                     falling back to the offline LexicalReranker"
                );
                Some(Arc::new(LexicalReranker::new()))
            }
        },
        RerankMode::Lexical => {
            tracing::info!(
                "using offline LexicalReranker (BM25-ish, no network) for retrieval reorder"
            );
            Some(Arc::new(LexicalReranker::new()))
        }
    }
}

/// The identity reranker. Exposed so callers that want an explicit no-op (rather
/// than `None`) can construct one without importing the core crate directly.
#[must_use]
pub fn noop_reranker() -> Arc<dyn Reranker> {
    Arc::new(NoopReranker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: RerankMode, key: Option<&str>) -> RerankerConfig {
        RerankerConfig {
            gateway_url: "https://example.test/v1".into(),
            gateway_key: key.map(str::to_string),
            model: DEFAULT_RERANK_MODEL.to_string(),
            mode,
        }
    }

    #[test]
    fn default_mode_is_off_yielding_no_reranker() {
        // The default (Off) ⇒ None, so retrieval behaves exactly as before. This
        // is what keeps the baseline tests green.
        assert!(build_reranker(&cfg(RerankMode::Off, Some("sk-test"))).is_none());
        assert!(build_reranker(&cfg(RerankMode::default(), None)).is_none());
    }

    #[test]
    fn gateway_mode_with_key_selects_a_reranker() {
        // gateway + key ⇒ Some (the real GatewayReranker — no network call is made
        // at construction, so this is a pure selection assertion).
        assert!(build_reranker(&cfg(RerankMode::Gateway, Some("sk-test"))).is_some());
    }

    #[test]
    fn gateway_mode_without_key_falls_back_to_lexical() {
        // gateway requested but no key ⇒ still Some (the offline LexicalReranker),
        // never None and never an unauthenticated gateway call.
        assert!(build_reranker(&cfg(RerankMode::Gateway, None)).is_some());
        assert!(build_reranker(&cfg(RerankMode::Gateway, Some("  "))).is_some());
    }

    #[test]
    fn lexical_mode_selects_a_reranker_without_a_key() {
        assert!(build_reranker(&cfg(RerankMode::Lexical, None)).is_some());
    }

    #[test]
    fn rerank_mode_parse() {
        assert_eq!(RerankMode::parse("gateway"), RerankMode::Gateway);
        assert_eq!(RerankMode::parse("ON"), RerankMode::Gateway);
        assert_eq!(RerankMode::parse("lexical"), RerankMode::Lexical);
        assert_eq!(RerankMode::parse("off"), RerankMode::Off);
        assert_eq!(RerankMode::parse(""), RerankMode::Off);
        assert_eq!(RerankMode::parse("nonsense"), RerankMode::Off);
    }
}
