//! Server configuration, read entirely from the environment.
//!
//! No secret is ever hardcoded. The gateway key is optional at *startup* — the
//! server still binds and answers protocol-only actions (`ping`,
//! `create_conversation_session`) without it — but `send_message` returns a
//! clean `error` event when the key is absent, so protocol conformance can be
//! tested with zero credentials.
//!
//! ## Environment variables (the contract every language E2E harness reuses)
//!
//! | var | default | meaning |
//! | --- | --- | --- |
//! | `SMOOTH_AGENT_BIND` | `127.0.0.1` | IP address to bind. Set `0.0.0.0` in k8s/containers so the Service/Ingress can reach the pod. |
//! | `SMOOTH_AGENT_PORT` | `8787` | TCP port to bind. |
//! | `SMOOAI_GATEWAY_URL` | `https://llm.smoo.ai/v1` | OpenAI-compatible LLM gateway base URL. |
//! | `SMOOAI_GATEWAY_KEY` | *(unset)* | Gateway API key. When unset, `send_message` errors cleanly. |
//! | `SMOOTH_AGENT_MODEL` | `claude-haiku-4-5` | Model id requested from the gateway. |
//! | `SMOOTH_AGENT_SEED_KB` | *(unset)* | When `1`, seed a couple of distinctive demo docs on startup. |
//! | `SMOOTH_AGENT_MAX_ITERATIONS` | `6` | Agent-loop iteration cap per turn. |
//! | `SMOOTH_AGENT_MAX_TOKENS` | `512` | `max_tokens` sent to the gateway (kept low — paid endpoint). |

use smooth_operator::llm::{ApiFormat, RetryPolicy};
use smooth_operator::LlmConfig;

/// Default bind address (loopback; override with `0.0.0.0` in containers).
pub const DEFAULT_BIND: &str = "127.0.0.1";
/// Default WebSocket bind port.
pub const DEFAULT_PORT: u16 = 8787;
/// Default OpenAI-compatible LLM gateway.
pub const DEFAULT_GATEWAY_URL: &str = "https://llm.smoo.ai/v1";
/// Default (cheap) model.
pub const DEFAULT_MODEL: &str = "claude-haiku-4-5";
/// Default agent-loop iteration cap.
pub const DEFAULT_MAX_ITERATIONS: u32 = 6;
/// Default `max_tokens` per LLM call.
pub const DEFAULT_MAX_TOKENS: u32 = 512;

/// Fully-resolved server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// IP address to bind (`127.0.0.1` for local dev, `0.0.0.0` in containers).
    pub bind: String,
    /// Port to bind.
    pub port: u16,
    /// LLM gateway base URL.
    pub gateway_url: String,
    /// Optional gateway API key. `None` means LLM turns are unavailable and
    /// `send_message` returns a clean error.
    pub gateway_key: Option<String>,
    /// Model id.
    pub model: String,
    /// Whether to seed the knowledge base with demo docs on startup.
    pub seed_kb: bool,
    /// Agent-loop iteration cap per turn.
    pub max_iterations: u32,
    /// `max_tokens` per LLM call.
    pub max_tokens: u32,
}

impl ServerConfig {
    /// Read configuration from the environment, applying documented defaults.
    #[must_use]
    pub fn from_env() -> Self {
        let bind = std::env::var("SMOOTH_AGENT_BIND")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BIND.to_string());

        let port = std::env::var("SMOOTH_AGENT_PORT")
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);

        let gateway_url = std::env::var("SMOOAI_GATEWAY_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string());

        let gateway_key = std::env::var("SMOOAI_GATEWAY_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let model = std::env::var("SMOOTH_AGENT_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let seed_kb = std::env::var("SMOOTH_AGENT_SEED_KB").as_deref() == Ok("1");

        let max_iterations = std::env::var("SMOOTH_AGENT_MAX_ITERATIONS")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_ITERATIONS);

        let max_tokens = std::env::var("SMOOTH_AGENT_MAX_TOKENS")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_TOKENS);

        Self {
            bind,
            port,
            gateway_url,
            gateway_key,
            model,
            seed_kb,
            max_iterations,
            max_tokens,
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

    #[test]
    fn defaults_apply_when_env_absent() {
        // Build a config directly (env-independent) to assert default constants
        // line up with the documented contract.
        let cfg = ServerConfig {
            bind: DEFAULT_BIND.to_string(),
            port: DEFAULT_PORT,
            gateway_url: DEFAULT_GATEWAY_URL.to_string(),
            gateway_key: None,
            model: DEFAULT_MODEL.to_string(),
            seed_kb: false,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_tokens: DEFAULT_MAX_TOKENS,
        };
        assert_eq!(cfg.port, 8787);
        assert_eq!(cfg.gateway_url, "https://llm.smoo.ai/v1");
        assert_eq!(cfg.model, "claude-haiku-4-5");
        assert!(!cfg.has_llm());
        assert!(cfg.llm_config().is_none());
    }

    #[test]
    fn llm_config_built_when_key_present() {
        let cfg = ServerConfig {
            bind: DEFAULT_BIND.to_string(),
            port: 1,
            gateway_url: "https://example.test/v1".into(),
            gateway_key: Some("sk-test".into()),
            model: "m".into(),
            seed_kb: false,
            max_iterations: 4,
            max_tokens: 128,
        };
        assert!(cfg.has_llm());
        let llm = cfg.llm_config().expect("llm config");
        assert_eq!(llm.api_url, "https://example.test/v1");
        assert_eq!(llm.model, "m");
        assert_eq!(llm.max_tokens, 128);
        assert!(matches!(llm.api_format, ApiFormat::OpenAiCompat));
    }
}
