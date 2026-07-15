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
//! | `SMOOTH_AGENT_PREAMBLE_MODEL` | *(unset → off)* | When set to a fast model id (e.g. `groq-gpt-oss-20b`), a small model runs in parallel with each streaming turn and emits ONE ephemeral `stream_preamble` sentence ("what I'm about to do") to cover the main model's time-to-first-token. Uses the same gateway/key as `SMOOTH_AGENT_MODEL`. Unset ⇒ no extra call, behavior unchanged. |
//! | `SMOOTH_AGENT_SEED_KB` | *(unset)* | When `1`, seed a couple of distinctive demo docs on startup. |
//! | `SMOOTH_AGENT_MAX_ITERATIONS` | `6` | Agent-loop iteration cap per turn. |
//! | `SMOOTH_AGENT_MAX_TOKENS` | `512` | `max_tokens` sent to the gateway (kept low — paid endpoint). |
//! | `SMOOTH_AGENT_STORAGE` | `memory` | Storage backend: `memory` \| `postgres` \| `dynamodb`. |
//! | `SMOOTH_AGENT_BACKPLANE` | `memory` | Connection backplane: `memory` (single-process) \| `redis`/`valkey` \| `nats`. A distributed backend is required for >1 replica and to let non-AI publishers push events via `Backplane::publish`. |
//! | `SMOOTH_AGENT_BACKPLANE_URL` | *(unset)* | Bus URL for `redis`/`nats` (e.g. `redis://valkey:6379`, `nats://nats:4222`); falls back to `SMOOTH_AGENT_REDIS_URL` / `SMOOTH_AGENT_NATS_URL`. |
//! | `WIDGET_AUTH_STRICT` | *(unset → `false`)* | Fail-closed embeddable-widget auth: when `1`/`true`, a session for an agent the [`WidgetAuthProvider`](smooth_operator::widget_auth::WidgetAuthProvider) has no policy for is rejected. Origin + `authContext` are always enforced for policied agents. |
//! | `SMOOTH_AGENT_CONFIRM_TOOLS` | *(unset → off)* | Comma-separated tool-name substrings that require **human confirmation** before the agent may run them (write-confirmation HITL). A turn that calls a matching tool parks and emits a `write_confirmation_required` event; the client resumes it with `confirm_tool_action` (`{sessionId, requestId, approved}`). Empty = no tool ever requires confirmation (byte-for-byte unchanged). |
//! | `WIDGET_AUTH_URL` | *(unset → permissive)* | When set, install an [`HttpWidgetAuth`](smooth_operator::widget_auth::HttpWidgetAuth) provider resolving each agent's embed policy from `{url}/{agentId}` — enforce widget auth against a host policy service with no custom binary. |
//! | `WIDGET_AUTH_BEARER` | *(unset)* | Optional bearer token sent to `WIDGET_AUTH_URL` (e.g. an M2M token). |
//! | `WIDGET_AUTH_TTL_SECS` | `60` | Policy cache TTL for `WIDGET_AUTH_URL` (incl. cached 404 no-policy results). |
//!
//! ### Auth (load-bearing — the admin API's `require_role` reads these)
//!
//! Parsed by [`smooth_operator::auth::AuthConfig::from_env`], not [`ServerConfig`],
//! but documented here because they gate `/admin` and the binary refuses to start
//! when they're misconfigured. See [`smooth_operator::auth`] for the full contract.
//!
//! | var | default | meaning |
//! | --- | --- | --- |
//! | `AUTH_MODE` | *(unset → admin disabled, 401)* | `jwt` (BYO) \| `smoo` (hosted) \| `none` (dev only). Unset boots `/ws` but `/admin` returns 401 until configured. |
//! | `AUTH_JWT_HS256_SECRET` | — | HS256 shared secret (for `jwt`/`smoo`). |
//! | `AUTH_JWT_RS256_PUBLIC_KEY` | — | RS256 PEM public key (takes precedence over HS256). |
//! | `AUTH_JWT_ISSUER` | — | Required `iss` claim (required for `smoo`; optional for `jwt`). |
//! | `AUTH_JWT_AUDIENCE` | — | Required `aud` claim (optional). |
//!
//! ### Embedding (the retrieval/index path)
//!
//! The `/index` path (and the `dev-support` example) select the embedder from the
//! gateway config above: with `SMOOAI_GATEWAY_KEY` set, the real **`GatewayEmbedder`**
//! (`text-embedding-3-small`, 1536-d) is used for semantic retrieval; without it,
//! the network-free **`DeterministicEmbedder`** (FNV-1a hash, 1024-d) is used and a
//! warning is logged. See [`crate::embedder`].

use smooth_operator_core::llm::{ApiFormat, RetryPolicy};
use smooth_operator_core::LlmConfig;

/// Default bind address (loopback; override with `0.0.0.0` in containers).
pub const DEFAULT_BIND: &str = "127.0.0.1";
/// Default WebSocket bind port.
pub const DEFAULT_PORT: u16 = 8787;
/// Default OpenAI-compatible LLM gateway.
pub const DEFAULT_GATEWAY_URL: &str = "https://llm.smoo.ai/v1";
/// Default (cheap) model.
pub const DEFAULT_MODEL: &str = "claude-haiku-4-5";
/// Default agent-loop iteration cap. Was 6 (chat-widget sizing) — too tight for
/// any multi-step turn. Raised to 20 for agentic use (EPIC th-1cc9fa).
pub const DEFAULT_MAX_ITERATIONS: u32 = 20;
/// Default `max_tokens` per LLM call. Was 512 (chat-widget sizing), which
/// STARVES reasoning models — they spend it all on `reasoning_content` and
/// return empty `content`. Raised to 8192 (EPIC th-1cc9fa). A cap only bounds
/// runaway output; concise answers stay concise, and the per-model output
/// ceiling clamp (`AgentConfig::with_model_ceiling`) keeps it under whatever the
/// model can physically emit.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Which storage backend the server runs on. Selected via `SMOOTH_AGENT_STORAGE`
/// (`memory` / `postgres` / `dynamodb`); the **admin stores** (connector configs,
/// settings, indexing runs) follow the same backend so they're durable wherever
/// the conversations / knowledge live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageBackend {
    /// Process-local in-memory (the default — local dev / tests). Admin stores
    /// are the in-memory impls (lost on restart).
    Memory,
    /// Postgres + pgvector. Admin stores persist to the same database.
    Postgres,
    /// DynamoDB single-table (AWS-serverless). Admin stores persist to the same
    /// table.
    Dynamodb,
}

impl StorageBackend {
    /// Parse from the `SMOOTH_AGENT_STORAGE` wire value (case-insensitive).
    /// Unknown / empty falls back to [`StorageBackend::Memory`].
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "postgres" | "pg" | "postgresql" => Self::Postgres,
            "dynamodb" | "ddb" | "dynamo" => Self::Dynamodb,
            _ => Self::Memory,
        }
    }
}

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
    /// Storage backend (drives both the storage adapter and the matching durable
    /// admin stores). Defaults to [`StorageBackend::Memory`].
    pub storage: StorageBackend,
    /// Fail-closed embeddable-widget auth: when `true`, a session for an agent
    /// the [`WidgetAuthProvider`](smooth_operator::widget_auth::WidgetAuthProvider)
    /// has **no** policy for is **rejected** (unknown/unregistered agents can't be
    /// embedded). When `false` (default), an absent policy is allowed — so the
    /// permissive default provider leaves `/ws` open. Set `WIDGET_AUTH_STRICT=1`
    /// in front of a real provider. Origin + `authContext` are always enforced
    /// for agents that *do* have a policy, regardless of this flag.
    pub widget_auth_strict: bool,
    /// **Write-confirmation HITL**: tool-name substrings that require human
    /// approval before the agent may run them. When non-empty, a turn that calls
    /// a matching tool **parks** and emits a `confirm_tool_action_required` event;
    /// the client resumes it with `confirm_tool_action`. Read from
    /// `SMOOTH_AGENT_CONFIRM_TOOLS` (comma-separated). Empty (the default) means
    /// no tool ever requires confirmation — no turn parks, byte-for-byte
    /// unchanged from before HITL. Matched by core's `ConfirmationHook` (`contains`).
    pub confirm_tools: Vec<String>,
    /// Cheap fast-tier model for the post-turn conversation-workflow judge
    /// (SMOODEV-590). Independent of [`model`](Self::model) so the judge stays
    /// cheap even when a turn runs on a bigger model. Read from
    /// `SMOOTH_AGENT_JUDGE_MODEL`; defaults to [`DEFAULT_MODEL`] (haiku-tier).
    pub judge_model: String,
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

        let storage = std::env::var("SMOOTH_AGENT_STORAGE")
            .ok()
            .map(|s| StorageBackend::parse(&s))
            .unwrap_or(StorageBackend::Memory);

        let widget_auth_strict = std::env::var("WIDGET_AUTH_STRICT")
            .ok()
            .map(|s| {
                let s = s.trim().to_ascii_lowercase();
                s == "1" || s == "true" || s == "yes"
            })
            .unwrap_or(false);

        let confirm_tools = std::env::var("SMOOTH_AGENT_CONFIRM_TOOLS")
            .ok()
            .map(|s| parse_confirm_tools(&s))
            .unwrap_or_default();

        let judge_model = std::env::var("SMOOTH_AGENT_JUDGE_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        Self {
            bind,
            port,
            gateway_url,
            gateway_key,
            model,
            seed_kb,
            max_iterations,
            max_tokens,
            storage,
            widget_auth_strict,
            confirm_tools,
            judge_model,
        }
    }

    /// The configured write-confirmation tool patterns, or `None` when none are
    /// configured (so the runner installs no `ConfirmationHook` and the turn
    /// behaves exactly as before HITL). `Some` only when at least one non-empty
    /// pattern is set.
    #[must_use]
    pub fn confirmation_tool_patterns(&self) -> Option<Vec<String>> {
        if self.confirm_tools.is_empty() {
            None
        } else {
            Some(self.confirm_tools.clone())
        }
    }

    /// `true` when a gateway key is present, so LLM turns can actually run.
    #[must_use]
    pub fn has_llm(&self) -> bool {
        self.gateway_key.is_some()
    }

    /// Build the smooth-operator [`LlmConfig`] for live turns using the server's
    /// configured (env) gateway key.
    ///
    /// Returns `None` when no gateway key is configured (callers should emit a
    /// clean protocol `error` rather than attempting a turn).
    ///
    /// In a multi-tenant flavor the per-turn key comes from a
    /// [`GatewayKeyResolver`](smooth_operator::gateway_key::GatewayKeyResolver)
    /// instead; use [`llm_config_with_key`](Self::llm_config_with_key) once the
    /// per-org key is resolved.
    #[must_use]
    pub fn llm_config(&self) -> Option<LlmConfig> {
        let key = self.gateway_key.clone()?;
        Some(self.llm_config_with_key(key))
    }

    /// Build the smooth-operator [`LlmConfig`] for live turns with an explicit
    /// gateway key (gateway URL, model, and limits still come from this config).
    ///
    /// This is the per-org seam's entry point: a
    /// [`GatewayKeyResolver`](smooth_operator::gateway_key::GatewayKeyResolver)
    /// resolves the key for the turn's org (falling back to the env key), and the
    /// resolved key is threaded through here. With the default env resolver this
    /// produces exactly the same config as [`llm_config`](Self::llm_config).
    #[must_use]
    pub fn llm_config_with_key(&self, key: String) -> LlmConfig {
        LlmConfig {
            api_url: self.gateway_url.clone(),
            api_key: key,
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            temperature: 0.0,
            retry_policy: RetryPolicy::default(),
            api_format: ApiFormat::OpenAiCompat,
        }
    }

    /// Build an [`LlmConfig`] **without** requiring a gateway key, for the
    /// test-only path where a [`MockLlmClient`](smooth_operator_core::llm_provider::MockLlmClient)
    /// is injected (the scenario-parity corpus). The mock replaces the client
    /// built from this config, so its url/key/model are never used to make a
    /// network call — this just satisfies the engine's `LlmConfig` argument so a
    /// keyless deterministic turn can run. Not reachable on the production path
    /// (only consulted when `chat_provider` is `Some`).
    #[must_use]
    pub fn placeholder_llm_config(&self) -> LlmConfig {
        LlmConfig {
            api_url: self.gateway_url.clone(),
            api_key: "mock-no-network".to_string(),
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            temperature: 0.0,
            retry_policy: RetryPolicy::default(),
            api_format: ApiFormat::OpenAiCompat,
        }
    }
}

/// Parse the comma-separated `SMOOTH_AGENT_CONFIRM_TOOLS` value into trimmed,
/// non-empty tool-name patterns. Whitespace-only / empty entries are dropped so
/// `","` or `" "` yields no patterns (HITL stays off).
fn parse_confirm_tools(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
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
            storage: StorageBackend::Memory,
            widget_auth_strict: false,
            confirm_tools: Vec::new(),
            judge_model: DEFAULT_MODEL.to_string(),
        };
        assert_eq!(cfg.port, 8787);
        assert_eq!(cfg.storage, StorageBackend::Memory);
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
            storage: StorageBackend::Memory,
            widget_auth_strict: false,
            confirm_tools: Vec::new(),
            judge_model: DEFAULT_MODEL.to_string(),
        };
        assert!(cfg.has_llm());
        let llm = cfg.llm_config().expect("llm config");
        assert_eq!(llm.api_url, "https://example.test/v1");
        assert_eq!(llm.model, "m");
        assert_eq!(llm.max_tokens, 128);
        assert!(matches!(llm.api_format, ApiFormat::OpenAiCompat));
    }

    #[test]
    fn storage_backend_parse_maps_aliases_and_defaults_memory() {
        assert_eq!(StorageBackend::parse("postgres"), StorageBackend::Postgres);
        assert_eq!(StorageBackend::parse("  PG "), StorageBackend::Postgres);
        assert_eq!(
            StorageBackend::parse("PostgreSQL"),
            StorageBackend::Postgres
        );
        assert_eq!(StorageBackend::parse("dynamodb"), StorageBackend::Dynamodb);
        assert_eq!(StorageBackend::parse("ddb"), StorageBackend::Dynamodb);
        assert_eq!(StorageBackend::parse("Dynamo"), StorageBackend::Dynamodb);
        // Memory is the default for the explicit value, unknown values, and empty.
        assert_eq!(StorageBackend::parse("memory"), StorageBackend::Memory);
        assert_eq!(StorageBackend::parse("sqlite"), StorageBackend::Memory);
        assert_eq!(StorageBackend::parse(""), StorageBackend::Memory);
    }
}
