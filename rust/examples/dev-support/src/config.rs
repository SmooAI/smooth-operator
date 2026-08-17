//! Configuration for the `dev-support` example: a `dev-support.toml` file (plus
//! a couple of environment overrides for secrets) describing **which GitHub
//! repo** to index, **how** to authenticate, **which agent** to run, and **which
//! tools** the agent gets.
//!
//! ## Shape (`dev-support.toml`)
//!
//! ```toml
//! [github]
//! owner = "rust-lang"
//! repo  = "mdBook"
//! # auth: "token" reads $GITHUB_TOKEN; "none" is unauthenticated public access.
//! auth  = "token"
//!
//! [github.include]
//! prose  = true   # READMEs, docs/, *.md
//! code   = true   # source files
//! issues = true   # issues + PRs
//!
//! [agent]
//! model         = "claude-haiku-4-5"
//! system_prompt = "You are the dev-team support agent for this repository. …"
//!
//! tools = ["knowledge_search", "github_search"]
//! ```
//!
//! ## Secrets come from the environment, never the file
//!
//! - `GITHUB_TOKEN` — the PAT used when `auth = "token"`.
//! - `SMOOAI_GATEWAY_KEY` — the key for the `llm.smoo.ai` gateway (consumed at
//!   `chat`/`serve` time, not stored in the config).
//!
//! Keeping secrets in the environment means the `dev-support.toml` is safe to
//! commit and share.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use smooth_operator_ingestion::{
    GithubAuth, GithubConnectorConfig, GithubInclude, GithubVisibility,
};

/// The default OpenAI-compatible gateway base URL (the live `llm.smoo.ai`
/// LiteLLM proxy). Overridable via `SMOOAI_GATEWAY_URL`.
pub const DEFAULT_GATEWAY_URL: &str = "https://llm.smoo.ai/v1";

/// The default model — the cheap, fast Haiku the rest of the workspace's live
/// tests use.
pub const DEFAULT_MODEL: &str = "claude-haiku-4-5";

/// The default system prompt for the dev-support agent. Keeps it grounded:
/// search the indexed repo first, reach for live GitHub when the index is stale.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are the dev-team knowledge & support agent for a \
    software project. You answer questions about the codebase, its documentation, and its issue \
    history. ALWAYS ground your answers in the repository: call `knowledge_search` to retrieve \
    indexed prose, code, and issues before answering anything repo-specific, and cite the source \
    files/issues you used. If the indexed snapshot might be stale (the user asks about \
    recently-merged code or a brand-new issue), call `github_search` for the live state. If you \
    cannot find an answer in the repository, say so plainly rather than guessing.";

/// How the connector + the `github_search` tool authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// A personal-access token read from `$GITHUB_TOKEN`.
    #[default]
    Token,
    /// No credentials — public repos at the anonymous rate limit.
    None,
}

/// Which content tiers to ingest.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct IncludeConfig {
    /// READMEs, `docs/`, `*.md` — the highest-signal knowledge.
    #[serde(default = "default_true")]
    pub prose: bool,
    /// Source files (extension allowlist; vendored/binary/lockfiles skipped).
    #[serde(default = "default_true")]
    pub code: bool,
    /// Issues + PRs (Q&A-style documents).
    #[serde(default = "default_true")]
    pub issues: bool,
}

fn default_true() -> bool {
    true
}

impl Default for IncludeConfig {
    fn default() -> Self {
        Self {
            prose: true,
            code: true,
            issues: true,
        }
    }
}

impl From<IncludeConfig> for GithubInclude {
    fn from(c: IncludeConfig) -> Self {
        GithubInclude {
            prose: c.prose,
            code: c.code,
            issues: c.issues,
        }
    }
}

/// The `[github]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubConfig {
    /// Repository owner (org or user).
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// How to authenticate (`token` ⇒ `$GITHUB_TOKEN`, `none` ⇒ public).
    #[serde(default)]
    pub auth: AuthMode,
    /// Which content tiers to pull.
    #[serde(default)]
    pub include: IncludeConfig,
    /// Whether the repo is private (drives ACL stamping on stored docs).
    #[serde(default)]
    pub private: bool,
}

/// The `[agent]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigSection {
    /// The model id requested from the gateway.
    #[serde(default = "default_model")]
    pub model: String,
    /// The agent's system prompt.
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
    /// Which tools the agent may call (defaults to both). Lives under `[agent]`
    /// so it reads naturally as `[agent] tools = […]` and isn't subject to
    /// TOML table-ordering surprises.
    #[serde(default = "default_tools")]
    pub tools: Vec<ToolName>,
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

fn default_system_prompt() -> String {
    DEFAULT_SYSTEM_PROMPT.to_string()
}

impl Default for AgentConfigSection {
    fn default() -> Self {
        Self {
            model: default_model(),
            system_prompt: default_system_prompt(),
            tools: default_tools(),
        }
    }
}

/// A tool the agent is allowed to call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    /// Search the indexed repo snapshot (RAG over the ingested corpus).
    KnowledgeSearch,
    /// Live GitHub code/issue search (fresh lookups beyond the index).
    GithubSearch,
}

fn default_tools() -> Vec<ToolName> {
    vec![ToolName::KnowledgeSearch, ToolName::GithubSearch]
}

/// The whole `dev-support.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevSupportConfig {
    /// The GitHub source.
    pub github: GithubConfig,
    /// The agent (model + prompt + enabled tools).
    #[serde(default)]
    pub agent: AgentConfigSection,
}

impl DevSupportConfig {
    /// Load + parse a `dev-support.toml` from `path`.
    ///
    /// # Errors
    /// Returns an error if the file can't be read or is not valid TOML for this
    /// schema.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        Self::from_toml_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))
    }

    /// Parse a config from a TOML string (the file-free seam, unit-tested).
    ///
    /// # Errors
    /// Returns an error if the string is not valid TOML for this schema.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| anyhow!("invalid dev-support config: {e}"))
    }

    /// The `owner/repo` slug, for display + tool scoping.
    #[must_use]
    pub fn repo_slug(&self) -> String {
        format!("{}/{}", self.github.owner, self.github.repo)
    }

    /// Whether the agent has a given tool enabled.
    #[must_use]
    pub fn has_tool(&self, tool: ToolName) -> bool {
        self.agent.tools.contains(&tool)
    }

    /// Resolve the GitHub auth from config + environment.
    ///
    /// `token` mode reads `$GITHUB_TOKEN`; an empty/missing token is an error in
    /// `token` mode (fail loud rather than silently fall back to the anonymous
    /// rate limit, which would surprise the user on a private repo). `none` mode
    /// is always [`GithubAuth::Unauthenticated`].
    ///
    /// # Errors
    /// Returns an error in `token` mode when `$GITHUB_TOKEN` is unset or empty.
    pub fn resolve_github_auth(&self) -> Result<GithubAuth> {
        match self.github.auth {
            AuthMode::None => Ok(GithubAuth::Unauthenticated),
            AuthMode::Token => {
                let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
                if token.trim().is_empty() {
                    return Err(anyhow!(
                        "github.auth is \"token\" but $GITHUB_TOKEN is unset/empty — export a \
                         personal-access token, or set github.auth = \"none\" for public repos"
                    ));
                }
                Ok(GithubAuth::Token(token))
            }
        }
    }

    /// Build the ingestion [`GithubConnectorConfig`] from this config + the
    /// resolved auth.
    ///
    /// # Errors
    /// Propagates [`resolve_github_auth`](Self::resolve_github_auth) failures.
    pub fn connector_config(&self) -> Result<GithubConnectorConfig> {
        let auth = self.resolve_github_auth()?;
        let visibility = if self.github.private {
            GithubVisibility::Private
        } else {
            GithubVisibility::Public
        };
        Ok(
            GithubConnectorConfig::new(&self.github.owner, &self.github.repo, auth)
                .include(self.github.include.into())
                .visibility(visibility),
        )
    }

    /// The org id used to scope ingested documents in the (multi-tenant)
    /// storage adapter — the repo slug is a natural, stable key.
    #[must_use]
    pub fn org_id(&self) -> String {
        self.repo_slug()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [github]
        owner = "rust-lang"
        repo = "mdBook"
        auth = "none"

        [github.include]
        prose = true
        code = false
        issues = true

        [agent]
        model = "claude-haiku-4-5"
        system_prompt = "Be helpful."
        tools = ["knowledge_search", "github_search"]
    "#;

    #[test]
    fn parses_full_config() {
        let cfg = DevSupportConfig::from_toml_str(SAMPLE).expect("parse");
        assert_eq!(cfg.github.owner, "rust-lang");
        assert_eq!(cfg.github.repo, "mdBook");
        assert_eq!(cfg.github.auth, AuthMode::None);
        assert!(cfg.github.include.prose);
        assert!(!cfg.github.include.code);
        assert!(cfg.github.include.issues);
        assert_eq!(cfg.agent.model, "claude-haiku-4-5");
        assert_eq!(cfg.repo_slug(), "rust-lang/mdBook");
        assert!(cfg.has_tool(ToolName::KnowledgeSearch));
        assert!(cfg.has_tool(ToolName::GithubSearch));
    }

    #[test]
    fn defaults_fill_in_missing_sections() {
        // Only the required [github] section is present.
        let cfg = DevSupportConfig::from_toml_str(
            r#"
            [github]
            owner = "o"
            repo = "r"
        "#,
        )
        .expect("parse minimal");
        // auth defaults to token, include defaults to all-on, agent defaults.
        assert_eq!(cfg.github.auth, AuthMode::Token);
        assert!(cfg.github.include.prose && cfg.github.include.code && cfg.github.include.issues);
        assert_eq!(cfg.agent.model, DEFAULT_MODEL);
        assert_eq!(cfg.agent.system_prompt, DEFAULT_SYSTEM_PROMPT);
        // tools default to both.
        assert_eq!(cfg.agent.tools.len(), 2);
    }

    #[test]
    fn none_auth_resolves_to_unauthenticated() {
        let cfg = DevSupportConfig::from_toml_str(SAMPLE).expect("parse");
        let auth = cfg.resolve_github_auth().expect("auth");
        assert!(matches!(auth, GithubAuth::Unauthenticated));
    }

    #[test]
    fn connector_config_maps_include_and_visibility() {
        let cfg = DevSupportConfig::from_toml_str(SAMPLE).expect("parse");
        let conn = cfg.connector_config().expect("connector config");
        assert_eq!(conn.owner, "rust-lang");
        assert_eq!(conn.repo, "mdBook");
        assert!(conn.include.prose);
        assert!(!conn.include.code);
        assert!(conn.include.issues);
        assert_eq!(conn.visibility, GithubVisibility::Public);
    }

    #[test]
    fn invalid_toml_is_an_error() {
        assert!(DevSupportConfig::from_toml_str("this is = not [valid").is_err());
        // Missing the required [github] section.
        assert!(DevSupportConfig::from_toml_str("[agent]\nmodel=\"x\"").is_err());
    }
}
