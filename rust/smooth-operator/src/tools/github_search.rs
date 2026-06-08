//! The `github_search` tool — live GitHub code/issue search.
//!
//! Where [`KnowledgeSearchTool`](crate::tools::KnowledgeSearchTool) searches the
//! *indexed snapshot* a `GithubConnector` ingested, `github_search` hits the
//! **live** GitHub search API so the agent can find code or issues that landed
//! after the last ingest — fresh lookups beyond the indexed corpus.
//!
//! ## Shape
//!
//! Arguments: `{ "query": string, "kind"?: "code" | "issues" }`. The tool runs
//! the query through a pluggable [`GithubSearchBackend`] (default:
//! [`OctocrabGithubSearch`], the real GitHub API) and renders the top results
//! (title, URL, snippet).
//!
//! ## Scope + auth
//!
//! The tool is constructed with a [`GithubAuth`] and a default `owner/repo`
//! scope; the scope is folded into the search query (`repo:owner/name`) so the
//! agent's lookups stay within the team's repos by default.
//!
//! ## Test split (G9)
//!
//! The live network is behind the [`OctocrabGithubSearch`] backend, exercised
//! only by an `#[ignore]` + env-gated (`SMOOTH_AGENT_E2E=1`) test. The tool's
//! arg-parsing and result-formatting are unit-tested **offline** against a stub
//! backend, exactly like the `web_search` tool.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use smooth_operator_core::tool::ToolSchema;
use smooth_operator_core::Tool;

/// Default number of results requested when the caller doesn't specify a limit.
const DEFAULT_RESULTS: usize = 5;
/// Hard cap on results regardless of what the model asks for.
const MAX_RESULTS: usize = 20;

/// How the `github_search` tool authenticates to the GitHub API.
///
/// Mirrors the ingestion connector's auth shape (kept independent so the tool
/// crate doesn't depend on the ingestion crate):
/// - [`GithubAuth::Token`] — a personal-access token (self-host's simplest path),
/// - [`GithubAuth::AppInstallation`] — Smoo's first-party GitHub App, the way
///   `lom.smoo.ai` wires per-customer access,
/// - [`GithubAuth::Unauthenticated`] — public search at the anonymous rate limit.
#[derive(Clone)]
pub enum GithubAuth {
    /// A personal-access token (PAT).
    Token(String),
    /// A GitHub App installation: app id, PEM private key, installation id.
    AppInstallation {
        /// The GitHub App's numeric id.
        app_id: u64,
        /// The App's RSA private key, in PEM form.
        private_key: String,
        /// The installation id (the org/user that installed the App).
        installation_id: u64,
    },
    /// No credentials (public search; anonymous rate limit).
    Unauthenticated,
}

impl std::fmt::Debug for GithubAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token(_) => f.write_str("GithubAuth::Token(***)"),
            Self::AppInstallation {
                app_id,
                installation_id,
                ..
            } => f
                .debug_struct("GithubAuth::AppInstallation")
                .field("app_id", app_id)
                .field("installation_id", installation_id)
                .field("private_key", &"***")
                .finish(),
            Self::Unauthenticated => f.write_str("GithubAuth::Unauthenticated"),
        }
    }
}

/// Which GitHub search index to query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubSearchKind {
    /// Search source code (`/search/code`).
    Code,
    /// Search issues + pull requests (`/search/issues`).
    Issues,
}

impl GithubSearchKind {
    /// Parse the `kind` argument; defaults to [`GithubSearchKind::Code`].
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::to_ascii_lowercase).as_deref() {
            Some("issue" | "issues" | "pr" | "prs") => Self::Issues,
            _ => Self::Code,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Issues => "issues",
        }
    }
}

/// A single GitHub search hit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GithubSearchResult {
    /// The result title (file path for code, issue title for issues).
    pub title: String,
    /// The result URL on github.com.
    pub url: String,
    /// A short snippet / summary.
    pub snippet: String,
}

impl GithubSearchResult {
    /// Convenience constructor.
    pub fn new(
        title: impl Into<String>,
        url: impl Into<String>,
        snippet: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
        }
    }
}

/// A pluggable GitHub-search backend.
///
/// The default [`OctocrabGithubSearch`] hits the real API. Tests inject a stub
/// so the tool's arg-parsing + formatting can be exercised offline.
#[async_trait]
pub trait GithubSearchBackend: Send + Sync {
    /// Run a search of `kind` for `query` (already scoped), up to `k` results.
    ///
    /// # Errors
    /// Returns an error if the upstream GitHub call fails (e.g. a 403 rate
    /// limit).
    async fn search(
        &self,
        query: &str,
        kind: GithubSearchKind,
        k: usize,
    ) -> anyhow::Result<Vec<GithubSearchResult>>;
}

/// Install the `ring` rustls `CryptoProvider` as the process default, once.
///
/// The workspace graph pulls in both `ring` and `aws-lc-rs`, so rustls 0.23
/// cannot auto-pick a provider and panics on first TLS use. We pin `ring`.
/// Idempotent — a second call (or a provider already installed elsewhere) is a
/// no-op.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// The real backend: live GitHub search via `octocrab`.
pub struct OctocrabGithubSearch {
    auth: GithubAuth,
}

impl OctocrabGithubSearch {
    /// Build the live backend over the given auth.
    #[must_use]
    pub fn new(auth: GithubAuth) -> Self {
        Self { auth }
    }

    fn client(&self) -> anyhow::Result<octocrab::Octocrab> {
        ensure_crypto_provider();
        let mut builder = octocrab::Octocrab::builder();
        builder = match &self.auth {
            GithubAuth::Token(token) => builder.personal_token(token.clone()),
            GithubAuth::AppInstallation {
                app_id,
                private_key,
                ..
            } => {
                let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_bytes())
                    .map_err(|e| anyhow::anyhow!("GitHub App private key invalid: {e}"))?;
                builder.app((*app_id).into(), key)
            }
            GithubAuth::Unauthenticated => builder,
        };
        let client = builder.build()?;
        if let GithubAuth::AppInstallation {
            installation_id, ..
        } = &self.auth
        {
            return Ok(client.installation((*installation_id).into())?);
        }
        Ok(client)
    }
}

#[async_trait]
impl GithubSearchBackend for OctocrabGithubSearch {
    async fn search(
        &self,
        query: &str,
        kind: GithubSearchKind,
        k: usize,
    ) -> anyhow::Result<Vec<GithubSearchResult>> {
        let client = self.client()?;
        match kind {
            GithubSearchKind::Code => {
                let page = client
                    .search()
                    .code(query)
                    .per_page(k as u8)
                    .send()
                    .await
                    .map_err(|e| map_github_err(e, "code"))?;
                Ok(page
                    .items
                    .into_iter()
                    .map(|item| {
                        GithubSearchResult::new(
                            item.path.clone(),
                            item.html_url.to_string(),
                            format!(
                                "{} in {}",
                                item.name,
                                item.repository.full_name.unwrap_or_default()
                            ),
                        )
                    })
                    .collect())
            }
            GithubSearchKind::Issues => {
                let page = client
                    .search()
                    .issues_and_pull_requests(query)
                    .per_page(k as u8)
                    .send()
                    .await
                    .map_err(|e| map_github_err(e, "issues"))?;
                Ok(page
                    .items
                    .into_iter()
                    .map(|item| {
                        let snippet = item.body.unwrap_or_default();
                        let snippet: String = snippet.chars().take(200).collect();
                        GithubSearchResult::new(item.title, item.html_url.to_string(), snippet)
                    })
                    .collect())
            }
        }
    }
}

/// Map an octocrab error into a clearer message, surfacing rate limits.
fn map_github_err(err: octocrab::Error, what: &str) -> anyhow::Error {
    let msg = err.to_string();
    if msg.contains("403") || msg.to_ascii_lowercase().contains("rate limit") {
        anyhow::anyhow!("GitHub {what} search hit a rate limit (HTTP 403): {msg}")
    } else {
        anyhow::anyhow!("GitHub {what} search failed: {msg}")
    }
}

/// A [`Tool`] that runs a live GitHub search through a [`GithubSearchBackend`],
/// scoped to a default `owner/repo`.
pub struct GithubSearchTool {
    backend: Arc<dyn GithubSearchBackend>,
    owner: String,
    repo: String,
}

impl GithubSearchTool {
    /// Build the tool over an auth and a default `owner/repo` scope, using the
    /// live [`OctocrabGithubSearch`] backend.
    #[must_use]
    pub fn new(auth: GithubAuth, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self::with_backend(Arc::new(OctocrabGithubSearch::new(auth)), owner, repo)
    }

    /// Build the tool over an explicit backend (tests inject a stub).
    #[must_use]
    pub fn with_backend(
        backend: Arc<dyn GithubSearchBackend>,
        owner: impl Into<String>,
        repo: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    /// Fold the default `repo:owner/name` scope into the user's query (unless
    /// they already pinned a `repo:`/`org:`/`user:` qualifier). Pure — unit
    /// tested offline.
    fn scoped_query(&self, query: &str) -> String {
        let lower = query.to_ascii_lowercase();
        if lower.contains("repo:") || lower.contains("org:") || lower.contains("user:") {
            query.to_string()
        } else {
            format!("{query} repo:{}/{}", self.owner, self.repo)
        }
    }
}

#[async_trait]
impl Tool for GithubSearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "github_search".to_string(),
            description: format!(
                "Search GitHub live for code or issues — fresh lookups beyond the indexed \
                 knowledge snapshot (newly-merged code, recent issues/PRs). Defaults to scoping \
                 results to the {}/{} repository; include a `repo:owner/name` qualifier in the \
                 query to search elsewhere. Use knowledge_search for already-indexed content; use \
                 this when you need the current state of the codebase or issue tracker. Returns \
                 results with title, URL, and snippet.",
                self.owner, self.repo
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The GitHub search query (GitHub search qualifiers allowed)."
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["code", "issues"],
                        "description": "Search source code ('code') or issues + PRs ('issues'). Defaults to 'code'."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results (default 5, max 20).",
                        "minimum": 1,
                        "maximum": 20
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<String> {
        let query = arguments
            .get("query")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("github_search requires a string 'query' argument"))?;

        let kind =
            GithubSearchKind::parse(arguments.get("kind").and_then(serde_json::Value::as_str));

        let k = arguments
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_RESULTS, |n| (n as usize).clamp(1, MAX_RESULTS));

        let scoped = self.scoped_query(query);
        let results = self.backend.search(&scoped, kind, k).await?;

        if results.is_empty() {
            return Ok(format!(
                "No GitHub {} results found for {scoped:?}.",
                kind.label()
            ));
        }

        let mut out = format!(
            "Found {} GitHub {} result(s) for {scoped:?}:\n",
            results.len(),
            kind.label()
        );
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. {} — {}\n   {}\n",
                i + 1,
                r.title,
                r.url,
                r.snippet
            ));
        }
        Ok(out)
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub backend recording the query/kind it was called with and returning
    /// canned hits — proves the tool's parse → scope → format path offline.
    struct StubBackend {
        last: std::sync::Mutex<Option<(String, GithubSearchKind, usize)>>,
    }

    impl StubBackend {
        fn new() -> Self {
            Self {
                last: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl GithubSearchBackend for StubBackend {
        async fn search(
            &self,
            query: &str,
            kind: GithubSearchKind,
            k: usize,
        ) -> anyhow::Result<Vec<GithubSearchResult>> {
            *self.last.lock().unwrap() = Some((query.to_string(), kind, k));
            Ok((0..k.min(2))
                .map(|i| {
                    GithubSearchResult::new(
                        format!("result-{i}.rs"),
                        format!("https://github.com/acme/app/blob/main/result-{i}.rs"),
                        format!("snippet {i}"),
                    )
                })
                .collect())
        }
    }

    fn tool() -> (GithubSearchTool, Arc<StubBackend>) {
        let backend = Arc::new(StubBackend::new());
        let tool = GithubSearchTool::with_backend(backend.clone(), "acme", "app");
        (tool, backend)
    }

    #[test]
    fn kind_parses_and_defaults_to_code() {
        assert_eq!(GithubSearchKind::parse(None), GithubSearchKind::Code);
        assert_eq!(
            GithubSearchKind::parse(Some("code")),
            GithubSearchKind::Code
        );
        assert_eq!(
            GithubSearchKind::parse(Some("issues")),
            GithubSearchKind::Issues
        );
        assert_eq!(
            GithubSearchKind::parse(Some("issue")),
            GithubSearchKind::Issues
        );
        assert_eq!(
            GithubSearchKind::parse(Some("PRs")),
            GithubSearchKind::Issues
        );
        // Unknown → code.
        assert_eq!(
            GithubSearchKind::parse(Some("nonsense")),
            GithubSearchKind::Code
        );
    }

    #[test]
    fn scoped_query_appends_repo_scope() {
        let (tool, _) = tool();
        assert_eq!(tool.scoped_query("foo bar"), "foo bar repo:acme/app");
    }

    #[test]
    fn scoped_query_respects_explicit_repo_qualifier() {
        let (tool, _) = tool();
        assert_eq!(
            tool.scoped_query("foo repo:other/thing"),
            "foo repo:other/thing"
        );
        assert_eq!(tool.scoped_query("bar org:acme"), "bar org:acme");
    }

    #[tokio::test]
    async fn execute_requires_query() {
        let (tool, _) = tool();
        let err = tool
            .execute(serde_json::json!({ "kind": "code" }))
            .await
            .expect_err("missing query should error");
        assert!(err.to_string().contains("query"));
    }

    #[tokio::test]
    async fn execute_scopes_query_and_formats_results() {
        let (tool, backend) = tool();
        let out = tool
            .execute(serde_json::json!({ "query": "fn main", "limit": 2 }))
            .await
            .expect("execute");

        // The backend saw the scoped query, code kind, k=2.
        let (q, kind, k) = backend
            .last
            .lock()
            .unwrap()
            .clone()
            .expect("backend called");
        assert_eq!(q, "fn main repo:acme/app");
        assert_eq!(kind, GithubSearchKind::Code);
        assert_eq!(k, 2);

        // The output renders the hits.
        assert!(out.contains("Found 2 GitHub code result(s)"), "got: {out}");
        assert!(out.contains("result-0.rs"), "got: {out}");
        assert!(
            out.contains("https://github.com/acme/app/blob/main/result-1.rs"),
            "got: {out}"
        );
        assert!(tool.is_read_only());
    }

    #[tokio::test]
    async fn execute_routes_issues_kind() {
        let (tool, backend) = tool();
        let out = tool
            .execute(serde_json::json!({ "query": "login broken", "kind": "issues" }))
            .await
            .expect("execute");
        let (_, kind, _) = backend.last.lock().unwrap().clone().unwrap();
        assert_eq!(kind, GithubSearchKind::Issues);
        assert!(out.contains("GitHub issues result(s)"), "got: {out}");
    }

    #[tokio::test]
    async fn execute_clamps_limit_to_max() {
        let (tool, backend) = tool();
        tool.execute(serde_json::json!({ "query": "x", "limit": 9999 }))
            .await
            .expect("execute");
        let (_, _, k) = backend.last.lock().unwrap().clone().unwrap();
        assert_eq!(k, MAX_RESULTS);
    }

    #[tokio::test]
    async fn empty_results_render_a_clear_message() {
        struct Empty;
        #[async_trait]
        impl GithubSearchBackend for Empty {
            async fn search(
                &self,
                _q: &str,
                _kind: GithubSearchKind,
                _k: usize,
            ) -> anyhow::Result<Vec<GithubSearchResult>> {
                Ok(vec![])
            }
        }
        let tool = GithubSearchTool::with_backend(Arc::new(Empty), "acme", "app");
        let out = tool
            .execute(serde_json::json!({ "query": "zzz" }))
            .await
            .unwrap();
        assert!(out.contains("No GitHub code results found"), "got: {out}");
    }

    #[test]
    fn auth_debug_never_leaks_secrets() {
        let token = GithubAuth::Token("ghp_secretvalue".to_string());
        assert!(!format!("{token:?}").contains("secretvalue"));
    }

    /// Live GitHub search — only with `SMOOTH_AGENT_E2E=1` (network). Run:
    /// `SMOOTH_AGENT_E2E=1 cargo test -p smooai-smooth-operator \
    ///    github_search::tests::live_search -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "network: gated on SMOOTH_AGENT_E2E"]
    async fn live_search() {
        if std::env::var("SMOOTH_AGENT_E2E").as_deref() != Ok("1") {
            eprintln!("skipping live GitHub search: set SMOOTH_AGENT_E2E=1 to run");
            return;
        }
        let auth = std::env::var("GITHUB_TOKEN")
            .map(GithubAuth::Token)
            .unwrap_or(GithubAuth::Unauthenticated);
        let tool = GithubSearchTool::new(auth, "rust-lang", "rust");
        let out = tool
            .execute(serde_json::json!({ "query": "fn main", "kind": "code", "limit": 3 }))
            .await
            .expect("live search");
        eprintln!("{out}");
        assert!(out.contains("github.com"), "expected GitHub URLs: {out}");
    }
}
