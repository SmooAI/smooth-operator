//! Lambda configuration, read entirely from the environment.
//!
//! No secret is ever hardcoded. The gateway key is optional — without it the
//! handler still answers protocol-only actions (`ping`,
//! `create_conversation_session`, `get_session`) and returns a clean
//! `LLM_UNAVAILABLE` error for `send_message`, so protocol conformance is
//! testable with zero credentials.
//!
//! ## Environment variables
//!
//! | var | default | meaning |
//! | --- | --- | --- |
//! | `SMOOTH_AGENT_DDB_TABLE` | `smooth-operator` | DynamoDB single-table name. Read directly by the adapter's `from_env`; mirrored here for the resolved view. |
//! | `SMOOAI_GATEWAY_URL` | `https://llm.smoo.ai/v1` | OpenAI-compatible LLM gateway base URL. |
//! | `SMOOAI_GATEWAY_KEY` | *(unset)* | Gateway API key. When unset, `send_message` errors cleanly. |
//! | `SMOOTH_AGENT_MODEL` | `gpt-6-luna` | Model id requested from the gateway. |
//! | `SMOOTH_AGENT_MAX_ITERATIONS` | `6` | Agent-loop iteration cap per turn. |
//! | `SMOOTH_AGENT_MAX_TOKENS` | `512` | `max_tokens` sent to the gateway. |
//! | `SMOOTH_AGENT_ORG_ID` | `default` | Org partition for knowledge + conversations. |
//! | `SMOOTH_AGENT_VECTOR_BUCKET` | *(unset)* | S3 Vectors bucket name. When set (with the index prefix), the S3 Vectors knowledge backend is used instead of brute-force DynamoDB. |
//! | `SMOOTH_AGENT_VECTOR_INDEX_PREFIX` | `smooth-agent-knowledge` | Prefix for per-org S3 Vectors index names (`{prefix}-{org}`). |

use smooth_operator_core::llm::{ApiFormat, RetryPolicy};
use smooth_operator_core::LlmConfig;

/// Default DynamoDB table name (matches the adapter's `DEFAULT_TABLE_NAME`).
pub const DEFAULT_TABLE_NAME: &str = "smooth-operator";
/// Default OpenAI-compatible LLM gateway.
pub const DEFAULT_GATEWAY_URL: &str = "https://llm.smoo.ai/v1";
/// Default model — the Smoo AI gateway's standard chat tier (SMOODEV-3342).
pub const DEFAULT_MODEL: &str = "gpt-6-luna";
/// Default agent-loop iteration cap.
pub const DEFAULT_MAX_ITERATIONS: u32 = 6;
/// Default `max_tokens` per LLM call.
pub const DEFAULT_MAX_TOKENS: u32 = 512;
/// Default org partition.
pub const DEFAULT_ORG_ID: &str = "default";
/// Default S3 Vectors index prefix.
pub const DEFAULT_VECTOR_INDEX_PREFIX: &str = "smooth-agent-knowledge";

/// Fully-resolved Lambda configuration.
#[derive(Debug, Clone)]
pub struct LambdaConfig {
    /// DynamoDB single-table name.
    pub table: String,
    /// LLM gateway base URL.
    pub gateway_url: String,
    /// Optional gateway API key. `None` means LLM turns are unavailable and
    /// `send_message` returns a clean error.
    pub gateway_key: Option<String>,
    /// Model id.
    pub model: String,
    /// Agent-loop iteration cap per turn.
    pub max_iterations: u32,
    /// `max_tokens` per LLM call.
    pub max_tokens: u32,
    /// Org partition for knowledge + conversations.
    pub org_id: String,
    /// When `Some`, the S3 Vectors knowledge backend (bucket + index prefix).
    pub vector_bucket: Option<String>,
    /// Index prefix for per-org S3 Vectors indexes.
    pub vector_index_prefix: String,
}

impl LambdaConfig {
    /// Read configuration from the environment, applying documented defaults.
    #[must_use]
    pub fn from_env() -> Self {
        let env = |key: &str| {
            std::env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };

        let table = env("SMOOTH_AGENT_DDB_TABLE").unwrap_or_else(|| DEFAULT_TABLE_NAME.to_string());
        let gateway_url =
            env("SMOOAI_GATEWAY_URL").unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string());
        let gateway_key = env("SMOOAI_GATEWAY_KEY");
        let model = env("SMOOTH_AGENT_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let max_iterations = env("SMOOTH_AGENT_MAX_ITERATIONS")
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_ITERATIONS);
        let max_tokens = env("SMOOTH_AGENT_MAX_TOKENS")
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_TOKENS);

        let org_id = env("SMOOTH_AGENT_ORG_ID").unwrap_or_else(|| DEFAULT_ORG_ID.to_string());
        let vector_bucket = env("SMOOTH_AGENT_VECTOR_BUCKET");
        let vector_index_prefix = env("SMOOTH_AGENT_VECTOR_INDEX_PREFIX")
            .unwrap_or_else(|| DEFAULT_VECTOR_INDEX_PREFIX.to_string());

        Self {
            table,
            gateway_url,
            gateway_key,
            model,
            max_iterations,
            max_tokens,
            org_id,
            vector_bucket,
            vector_index_prefix,
        }
    }

    /// `true` when a gateway key is present, so LLM turns can actually run.
    #[must_use]
    pub fn has_llm(&self) -> bool {
        self.gateway_key.is_some()
    }

    /// Build the smooth-operator [`LlmConfig`] for live turns.
    ///
    /// Returns `None` when no gateway key is configured (callers should emit a
    /// clean protocol `error` rather than attempting a turn).
    #[must_use]
    pub fn llm_config(&self) -> Option<LlmConfig> {
        let key = self.gateway_key.clone()?;
        Some(LlmConfig {
            api_url: self.gateway_url.clone(),
            api_key: key,
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            temperature: 0.0,
            retry_policy: RetryPolicy::default(),
            api_format: ApiFormat::OpenAiCompat,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SMOODEV-3342: the Lambda host defaults to the gateway's gpt-6-luna tier,
    /// in lockstep with `smooth-operator-server`.
    #[test]
    fn default_model_is_gpt_6_luna() {
        assert_eq!(DEFAULT_MODEL, "gpt-6-luna");
    }

    #[test]
    fn llm_config_absent_without_key() {
        let cfg = LambdaConfig {
            table: DEFAULT_TABLE_NAME.into(),
            gateway_url: DEFAULT_GATEWAY_URL.into(),
            gateway_key: None,
            model: DEFAULT_MODEL.into(),
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_tokens: DEFAULT_MAX_TOKENS,
            org_id: DEFAULT_ORG_ID.into(),
            vector_bucket: None,
            vector_index_prefix: DEFAULT_VECTOR_INDEX_PREFIX.into(),
        };
        assert!(!cfg.has_llm());
        assert!(cfg.llm_config().is_none());
    }

    #[test]
    fn llm_config_built_when_key_present() {
        let cfg = LambdaConfig {
            table: "t".into(),
            gateway_url: "https://example.test/v1".into(),
            gateway_key: Some("sk-test".into()),
            model: "m".into(),
            max_iterations: 4,
            max_tokens: 128,
            org_id: "org".into(),
            vector_bucket: Some("vb".into()),
            vector_index_prefix: "idx".into(),
        };
        let llm = cfg.llm_config().expect("llm config");
        assert_eq!(llm.api_url, "https://example.test/v1");
        assert_eq!(llm.model, "m");
        assert_eq!(llm.max_tokens, 128);
        assert!(matches!(llm.api_format, ApiFormat::OpenAiCompat));
    }
}
