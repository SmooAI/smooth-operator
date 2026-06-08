//! [`GithubConnector`] — pull a GitHub repository's knowledge into
//! [`RawDocument`]s: prose (READMEs / `docs/` / `*.md`), source code, and
//! issues / PRs / discussions.
//!
//! This is the primary source behind the `examples/dev-support` dev-team
//! knowledge agent (Onyx-gap G1). It mirrors the [`FileConnector`]/[`WebConnector`]
//! shape: a [`Connector`] impl plus an offline contract test (a mock GitHub API
//! via `wiremock`, see `tests/github_connector.rs`) and an `external`-gated live
//! test.
//!
//! ## Content types → `RawDocument`
//!
//! | GitHub content                         | `metadata.kind` | `source`            |
//! | -------------------------------------- | --------------- | ------------------- |
//! | `README*`, `docs/**`, `*.md` / `*.mdx` | `prose`         | blob URL            |
//! | source files (extension allowlist)     | `code`          | blob URL            |
//! | issues / PRs / discussions             | `issue`/`pr`/…  | issue/PR URL        |
//!
//! Every document carries rich metadata (`repo`, `path`, `url`, `updated_at`,
//! `lang`/`state`/`labels`) so retrieval and citations can attribute it.
//!
//! ## Auth ([`GithubAuth`])
//!
//! - [`GithubAuth::Token`] — a personal-access token (PAT). The simplest
//!   self-host path: bring your own token.
//! - [`GithubAuth::AppInstallation`] — a GitHub App installation (app id +
//!   PEM private key + installation id). This is how **lom.smoo.ai** wires
//!   Smoo's GitHub App: the platform owns one App, and each customer installs
//!   it on their org so Smoo can index their repos without sharing a PAT. A
//!   self-hosted deployment can use either path.
//! - [`GithubAuth::Unauthenticated`] — no credentials (public repos / the mock
//!   contract test). Subject to GitHub's anonymous rate limit in production.
//!
//! ## ACL
//!
//! Repo visibility maps to the document ACL the pipeline stamps (`DocAcl`): a
//! **private** repo's documents get an ACL scoping them to the configured group
//! entitlement ([`GithubConnectorConfig::acl_groups`]); a **public** repo's
//! documents are left org-public (no ACL).
//!
//! ## Incremental ([`Connector::pull`] `since`)
//!
//! `pull(since)` passes `since` to the GitHub issues API's `since` filter (only
//! issues/PRs updated at/after the watermark are returned) and carries each
//! document's `updated_at` through metadata. Content-level idempotency is
//! handled downstream by the pipeline's `(id, content-hash)` ledger.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;

use crate::connector::{Connector, RawDocument, Timestamp};

/// Install the `ring` rustls [`CryptoProvider`](rustls::crypto::CryptoProvider)
/// as the process default, once.
///
/// The workspace graph pulls in **both** `ring` (via octocrab/reqwest) and
/// `aws-lc-rs` (via the OTLP/tonic stack), so rustls 0.23 cannot auto-pick a
/// process-level provider and panics on first TLS use. We pin `ring`
/// explicitly. Idempotent: a second call (or a provider already installed by
/// another component) is a no-op.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Ignore the error: if another component already installed a provider,
        // that's fine — we only need *a* provider to exist.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Default source-file extensions treated as ingestible code. Lockfiles,
/// binaries, and vendored trees are excluded by [`is_vendored_path`] /
/// [`looks_binary`] regardless of this list.
pub const DEFAULT_CODE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "kt", "rb", "php", "cs", "cpp", "cc", "c",
    "h", "hpp", "swift", "scala", "sh", "bash", "sql", "yaml", "yml", "toml", "json", "html",
    "css",
];

/// Default cap on a single code/prose file's size (bytes). Files larger than
/// this are skipped (the chunker handles in-file splitting; this guards against
/// pulling a multi-megabyte generated file).
pub const DEFAULT_MAX_FILE_BYTES: usize = 512 * 1024;

/// How a connector authenticates to the GitHub API.
#[derive(Clone)]
pub enum GithubAuth {
    /// A personal-access token (PAT). Self-host's simplest path.
    Token(String),
    /// A GitHub App installation: app id, PEM private key, installation id.
    /// How `lom.smoo.ai` wires Smoo's first-party GitHub App per customer org.
    AppInstallation {
        /// The GitHub App's numeric id.
        app_id: u64,
        /// The App's RSA private key, in PEM form.
        private_key: String,
        /// The installation id (the org/user that installed the App).
        installation_id: u64,
    },
    /// No credentials — public repos and the offline contract test. Subject to
    /// GitHub's anonymous rate limit in production.
    Unauthenticated,
}

impl std::fmt::Debug for GithubAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print secret material.
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

/// A repository's visibility — drives whether documents get a restricting ACL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubVisibility {
    /// Public repo: documents are org-public (no ACL stamped).
    Public,
    /// Private repo: documents are scoped to the configured ACL groups.
    Private,
}

/// Which content types the connector pulls.
#[derive(Debug, Clone, Copy)]
pub struct GithubInclude {
    /// READMEs, `docs/**`, and `*.md` / `*.mdx` (the highest-signal knowledge).
    pub prose: bool,
    /// Source files (extension allowlist; vendored/binary/lockfiles skipped).
    pub code: bool,
    /// Issues, PRs, and discussions (Q&A-style documents).
    pub issues: bool,
}

impl Default for GithubInclude {
    fn default() -> Self {
        Self {
            prose: true,
            code: true,
            issues: true,
        }
    }
}

/// Configuration for a [`GithubConnector`].
#[derive(Debug, Clone)]
pub struct GithubConnectorConfig {
    /// Repository owner (org or user).
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// How to authenticate.
    pub auth: GithubAuth,
    /// Which content types to pull.
    pub include: GithubInclude,
    /// Branch / tag / sha to read. `None` → the repo's default branch.
    pub r#ref: Option<String>,
    /// Source-file extensions treated as ingestible code.
    pub code_extensions: Vec<String>,
    /// Cap on a single file's size (bytes); larger files are skipped.
    pub max_file_bytes: usize,
    /// Repository visibility (drives ACL stamping).
    pub visibility: GithubVisibility,
    /// ACL group entitlements stamped on private-repo documents.
    pub acl_groups: Vec<String>,
    /// Override the GitHub API base URL (tests point this at a mock server).
    /// `None` → `https://api.github.com`.
    pub base_uri: Option<String>,
    /// Max API pages to fetch per paginated list (politeness / safety cap).
    pub max_pages: u32,
}

impl GithubConnectorConfig {
    /// Build a config for `owner/repo` with the given auth and sensible
    /// defaults (all content types, default branch, default extension
    /// allowlist + size cap, public visibility).
    #[must_use]
    pub fn new(owner: impl Into<String>, repo: impl Into<String>, auth: GithubAuth) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            auth,
            include: GithubInclude::default(),
            r#ref: None,
            code_extensions: DEFAULT_CODE_EXTENSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            visibility: GithubVisibility::Public,
            acl_groups: Vec::new(),
            base_uri: None,
            max_pages: 10,
        }
    }

    /// Point the connector at a custom GitHub API base URL (builder; tests use
    /// this to target a mock server).
    #[must_use]
    pub fn base_uri(mut self, base_uri: impl Into<String>) -> Self {
        self.base_uri = Some(base_uri.into());
        self
    }

    /// Set the repository visibility (builder).
    #[must_use]
    pub fn visibility(mut self, visibility: GithubVisibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// Set the ACL group entitlements for a private repo (builder).
    #[must_use]
    pub fn acl_groups<I, S>(mut self, groups: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.acl_groups = groups.into_iter().map(Into::into).collect();
        self
    }

    /// Pin the git ref (branch/tag/sha) to read (builder).
    #[must_use]
    pub fn at_ref(mut self, r#ref: impl Into<String>) -> Self {
        self.r#ref = Some(r#ref.into());
        self
    }

    /// Override which content types are pulled (builder).
    #[must_use]
    pub fn include(mut self, include: GithubInclude) -> Self {
        self.include = include;
        self
    }

    /// Override the code-file extension allowlist (builder).
    #[must_use]
    pub fn code_extensions<I, S>(mut self, exts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.code_extensions = exts.into_iter().map(Into::into).collect();
        self
    }

    /// Cap a single file's size in bytes (builder).
    #[must_use]
    pub fn max_file_bytes(mut self, bytes: usize) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// The ACL labels to stamp on documents, or `None` for a public repo.
    fn acl(&self) -> Option<Vec<String>> {
        match self.visibility {
            GithubVisibility::Private if !self.acl_groups.is_empty() => {
                Some(self.acl_groups.clone())
            }
            // A private repo with no configured groups still must not be
            // org-public — scope it to a synthetic per-repo group so it is never
            // accidentally readable org-wide.
            GithubVisibility::Private => Some(vec![format!("github:{}/{}", self.owner, self.repo)]),
            GithubVisibility::Public => None,
        }
    }
}

/// Pulls a GitHub repository's prose, code, and issues as [`RawDocument`]s.
pub struct GithubConnector {
    config: GithubConnectorConfig,
}

impl GithubConnector {
    /// Build a connector from its config.
    #[must_use]
    pub fn new(config: GithubConnectorConfig) -> Self {
        Self { config }
    }

    /// Build the authenticated `octocrab` client (pointed at the configured
    /// base URL, or `api.github.com`).
    fn client(&self) -> Result<octocrab::Octocrab> {
        ensure_crypto_provider();
        let mut builder = octocrab::Octocrab::builder();
        if let Some(base) = &self.config.base_uri {
            builder = builder
                .base_uri(base.clone())
                .with_context(|| format!("invalid GitHub base URI {base:?}"))?;
        }
        builder = match &self.config.auth {
            GithubAuth::Token(token) => builder.personal_token(token.clone()),
            GithubAuth::AppInstallation {
                app_id,
                private_key,
                ..
            } => {
                let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_bytes())
                    .context("GitHub App private key is not a valid RSA PEM")?;
                builder.app((*app_id).into(), key)
            }
            GithubAuth::Unauthenticated => builder,
        };
        let client = builder.build().context("building GitHub API client")?;

        // A GitHub App's app-JWT client must scope down to the installation to
        // act on its repos. (No-op for token / unauthenticated.)
        if let GithubAuth::AppInstallation {
            installation_id, ..
        } = &self.config.auth
        {
            return client
                .installation((*installation_id).into())
                .with_context(|| format!("scoping to installation {installation_id}"));
        }
        Ok(client)
    }

    /// Resolve the ref to read: the configured one, or the repo's default
    /// branch (fetched once).
    async fn resolve_ref(&self, client: &octocrab::Octocrab) -> Result<String> {
        if let Some(r) = &self.config.r#ref {
            return Ok(r.clone());
        }
        let route = format!("/repos/{}/{}", self.config.owner, self.config.repo);
        let repo: Value = client
            .get(&route, None::<&()>)
            .await
            .with_context(|| format!("fetching repo metadata {route}"))?;
        Ok(repo
            .get("default_branch")
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_string())
    }

    /// List every blob path in the repo tree at `git_ref` (recursive).
    async fn list_tree(&self, client: &octocrab::Octocrab, git_ref: &str) -> Result<Vec<String>> {
        let route = format!(
            "/repos/{}/{}/git/trees/{}?recursive=1",
            self.config.owner, self.config.repo, git_ref
        );
        let tree: Value = client
            .get(&route, None::<&()>)
            .await
            .with_context(|| format!("fetching git tree {route}"))?;

        let entries = tree
            .get("tree")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut paths = Vec::new();
        for entry in entries {
            let is_blob = entry.get("type").and_then(Value::as_str) == Some("blob");
            if !is_blob {
                continue;
            }
            if let Some(path) = entry.get("path").and_then(Value::as_str) {
                paths.push(path.to_string());
            }
        }
        Ok(paths)
    }

    /// Fetch + decode a single blob's text content from the contents API.
    /// Returns `Ok(None)` if the blob is too large or its content is missing.
    async fn fetch_blob(
        &self,
        client: &octocrab::Octocrab,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<(String, String)>> {
        let route = format!(
            "/repos/{}/{}/contents/{}?ref={}",
            self.config.owner, self.config.repo, path, git_ref
        );
        let blob: Value = client
            .get(&route, None::<&()>)
            .await
            .with_context(|| format!("fetching blob {route}"))?;

        let size = blob.get("size").and_then(Value::as_u64).unwrap_or(0) as usize;
        if size > self.config.max_file_bytes {
            return Ok(None);
        }

        let html_url = blob
            .get("html_url")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| self.blob_url(path, git_ref));

        let Some(raw) = blob.get("content").and_then(Value::as_str) else {
            return Ok(None);
        };
        // The contents API base64-encodes with embedded newlines.
        let cleaned: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(cleaned.as_bytes())
            .with_context(|| format!("decoding base64 blob {path}"))?;
        if bytes.len() > self.config.max_file_bytes {
            return Ok(None);
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| anyhow!("blob {path} is not valid UTF-8 (likely binary)"))?;
        Ok(Some((text, html_url)))
    }

    /// The canonical blob URL for `path` at `git_ref` (citation source).
    fn blob_url(&self, path: &str, git_ref: &str) -> String {
        format!(
            "https://github.com/{}/{}/blob/{}/{}",
            self.config.owner, self.config.repo, git_ref, path
        )
    }

    fn repo_slug(&self) -> String {
        format!("{}/{}", self.config.owner, self.config.repo)
    }

    /// Pull prose + code documents from the repo tree.
    async fn pull_files(
        &self,
        client: &octocrab::Octocrab,
        git_ref: &str,
    ) -> Result<Vec<RawDocument>> {
        let paths = self.list_tree(client, git_ref).await?;
        let mut docs = Vec::new();
        for path in paths {
            let classification = classify_path(path.as_str(), &self.config.code_extensions);
            let kind = match classification {
                PathKind::Prose if self.config.include.prose => "prose",
                PathKind::Code if self.config.include.code => "code",
                _ => continue,
            };
            let Some((content, html_url)) = self.fetch_blob(client, &path, git_ref).await? else {
                continue;
            };
            if content.trim().is_empty() {
                continue;
            }

            let mut doc = RawDocument::new(
                format!("{}@{}#{}", self.repo_slug(), git_ref, path),
                // `source` is what the pipeline stamps as the stored Document's
                // source — the GitHub blob URL, so citations link to GitHub.
                html_url.clone(),
                content,
            )
            .with_title(path.clone())
            .with_metadata("kind", kind)
            .with_metadata("repo", self.repo_slug())
            .with_metadata("path", path.clone())
            .with_metadata("url", html_url)
            .with_metadata("ref", git_ref.to_string());

            if kind == "code" {
                if let Some(lang) = lang_for_path(&path) {
                    doc = doc.with_metadata("lang", lang);
                }
            }
            if let Some(acl) = self.config.acl() {
                doc = doc.with_acl(acl);
            }
            docs.push(doc);
        }
        Ok(docs)
    }

    /// Pull issues + PRs (the issues API returns both; PRs carry a
    /// `pull_request` field) as Q&A-style documents.
    async fn pull_issues(
        &self,
        client: &octocrab::Octocrab,
        since: Option<Timestamp>,
    ) -> Result<Vec<RawDocument>> {
        let mut route = format!(
            "/repos/{}/{}/issues?state=all&per_page=50",
            self.config.owner, self.config.repo
        );
        if let Some(since) = since {
            route.push_str(&format!("&since={}", since.to_rfc3339()));
        }

        let issues: Value = client
            .get(&route, None::<&()>)
            .await
            .with_context(|| format!("fetching issues {route}"))?;
        let items = issues.as_array().cloned().unwrap_or_default();

        let mut docs = Vec::new();
        for item in items {
            let Some(doc) = self.issue_to_doc(client, &item).await? else {
                continue;
            };
            docs.push(doc);
        }
        Ok(docs)
    }

    /// Shape a single issue/PR JSON value into a [`RawDocument`], fetching its
    /// top comments. Returns `Ok(None)` if the value lacks a number/url.
    async fn issue_to_doc(
        &self,
        client: &octocrab::Octocrab,
        item: &Value,
    ) -> Result<Option<RawDocument>> {
        let Some(number) = item.get("number").and_then(Value::as_u64) else {
            return Ok(None);
        };
        let url = item
            .get("html_url")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("https://github.com/{}/issues/{number}", self.repo_slug()));
        let kind = if item.get("pull_request").is_some() {
            "pr"
        } else {
            "issue"
        };

        // Fetch top comments (best-effort: a comments failure shouldn't drop the
        // whole issue — index the title+body alone).
        let comments = self
            .fetch_issue_comments(client, number)
            .await
            .unwrap_or_default();

        let mut doc = shape_issue_document(item, number, &url, kind, &self.repo_slug(), &comments);
        if let Some(acl) = self.config.acl() {
            doc = doc.with_acl(acl);
        }
        Ok(Some(doc))
    }

    /// Fetch up to the first page of an issue's comment bodies.
    async fn fetch_issue_comments(
        &self,
        client: &octocrab::Octocrab,
        number: u64,
    ) -> Result<Vec<String>> {
        let route = format!(
            "/repos/{}/{}/issues/{number}/comments?per_page=20",
            self.config.owner, self.config.repo
        );
        let comments: Value = client.get(&route, None::<&()>).await?;
        Ok(comments
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.get("body").and_then(Value::as_str))
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// How a repo path is classified for ingestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    /// README / docs/ / *.md / *.mdx — the prose tier.
    Prose,
    /// A source file in the extension allowlist.
    Code,
    /// Skipped (vendored, binary, lockfile, or unknown extension).
    Skip,
}

/// Classify a repo path into [`PathKind`], applying the vendored/binary/lockfile
/// skip rules first. Pure (no I/O) so it's unit-testable.
fn classify_path(path: &str, code_extensions: &[String]) -> PathKind {
    if is_vendored_path(path) || looks_binary(path) || is_lockfile(path) {
        return PathKind::Skip;
    }
    if is_prose_path(path) {
        return PathKind::Prose;
    }
    let ext = extension_of(path);
    if let Some(ext) = ext {
        if code_extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
            return PathKind::Code;
        }
    }
    PathKind::Skip
}

/// Prose: a README anywhere, anything under `docs/`, or a markdown file.
fn is_prose_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let basename = lower.rsplit('/').next().unwrap_or(&lower);
    basename.starts_with("readme")
        || lower == "docs"
        || lower.starts_with("docs/")
        || lower.contains("/docs/")
        || basename.ends_with(".md")
        || basename.ends_with(".mdx")
        || basename.ends_with(".markdown")
}

/// Vendored / build-output trees that should never be indexed.
fn is_vendored_path(path: &str) -> bool {
    const VENDORED: &[&str] = &[
        "node_modules/",
        "vendor/",
        "target/",
        "dist/",
        "build/",
        ".git/",
        "__pycache__/",
        ".next/",
        "venv/",
        ".venv/",
    ];
    let p = path.trim_start_matches("./");
    VENDORED.iter().any(|seg| {
        p == seg.trim_end_matches('/') || p.starts_with(seg) || p.contains(&format!("/{seg}"))
    })
}

/// Lockfiles carry no knowledge — skip them.
fn is_lockfile(path: &str) -> bool {
    const LOCKFILES: &[&str] = &[
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "cargo.lock",
        "poetry.lock",
        "composer.lock",
        "gemfile.lock",
        "go.sum",
        "uv.lock",
    ];
    let basename = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    LOCKFILES.contains(&basename.as_str())
}

/// Binary / asset extensions that are not text knowledge.
fn looks_binary(path: &str) -> bool {
    const BINARY_EXTS: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "webp", "svg", "ico", "pdf", "zip", "gz", "tar", "bz2", "7z",
        "mp4", "mov", "mp3", "wav", "woff", "woff2", "ttf", "eot", "otf", "wasm", "so", "dylib",
        "dll", "exe", "bin", "class", "jar", "o", "a", "lib", "pyc",
    ];
    match extension_of(path) {
        Some(ext) => BINARY_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// The lowercase extension of a path, if any.
fn extension_of(path: &str) -> Option<&str> {
    let basename = path.rsplit('/').next().unwrap_or(path);
    basename.rsplit_once('.').map(|(_, ext)| ext)
}

/// Map a code file's extension to a human language label (citation metadata).
fn lang_for_path(path: &str) -> Option<&'static str> {
    let ext = extension_of(path)?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "rb" => "ruby",
        "php" => "php",
        "cs" => "csharp",
        "cpp" | "cc" | "c" | "h" | "hpp" => "c/c++",
        "swift" => "swift",
        "scala" => "scala",
        "sh" | "bash" => "shell",
        "sql" => "sql",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "json" => "json",
        "html" => "html",
        "css" => "css",
        _ => return None,
    })
}

/// Build a Q&A-style [`RawDocument`] from an issue/PR JSON value + comment
/// bodies. Pure (no I/O) so the shaping is unit-testable offline.
fn shape_issue_document(
    item: &Value,
    number: u64,
    url: &str,
    kind: &str,
    repo_slug: &str,
    comments: &[String],
) -> RawDocument {
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let body = item.get("body").and_then(Value::as_str).unwrap_or("");
    let state = item
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let updated_at = item
        .get("updated_at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let labels: Vec<String> = item
        .get("labels")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();

    // Concatenate title + body + top comments into one searchable document.
    let mut content = format!("{title}\n\n{body}");
    for (i, comment) in comments.iter().enumerate() {
        content.push_str(&format!("\n\nComment {}: {comment}", i + 1));
    }

    let mut doc = RawDocument::new(
        format!("{repo_slug}#{kind}-{number}"),
        url.to_string(),
        content,
    )
    .with_title(format!("{kind} #{number}: {title}"))
    .with_metadata("kind", kind)
    .with_metadata("repo", repo_slug)
    .with_metadata("url", url)
    .with_metadata("number", number.to_string())
    .with_metadata("state", state);
    if !updated_at.is_empty() {
        doc = doc.with_metadata("updated_at", updated_at);
    }
    if !labels.is_empty() {
        doc = doc.with_metadata("labels", labels.join(","));
    }
    doc
}

#[async_trait]
impl Connector for GithubConnector {
    fn name(&self) -> &str {
        "github"
    }

    async fn pull(&self, since: Option<Timestamp>) -> Result<Vec<RawDocument>> {
        let client = self.client()?;
        let git_ref = self.resolve_ref(&client).await?;

        let mut docs = Vec::new();
        if self.config.include.prose || self.config.include.code {
            docs.extend(
                self.pull_files(&client, &git_ref).await.with_context(|| {
                    format!("pulling files from {}@{git_ref}", self.repo_slug())
                })?,
            );
        }
        if self.config.include.issues {
            docs.extend(
                self.pull_issues(&client, since)
                    .await
                    .with_context(|| format!("pulling issues from {}", self.repo_slug()))?,
            );
        }
        Ok(docs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- path classification (pure, offline) ------------------------------

    fn exts() -> Vec<String> {
        DEFAULT_CODE_EXTENSIONS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    #[test]
    fn classify_prose_paths() {
        assert_eq!(classify_path("README.md", &exts()), PathKind::Prose);
        assert_eq!(classify_path("readme", &exts()), PathKind::Prose);
        assert_eq!(
            classify_path("docs/architecture.md", &exts()),
            PathKind::Prose
        );
        assert_eq!(classify_path("guide/notes.mdx", &exts()), PathKind::Prose);
        assert_eq!(classify_path("sub/docs/x.md", &exts()), PathKind::Prose);
    }

    #[test]
    fn classify_code_paths() {
        assert_eq!(classify_path("src/lib.rs", &exts()), PathKind::Code);
        assert_eq!(classify_path("app/main.py", &exts()), PathKind::Code);
        assert_eq!(classify_path("web/index.ts", &exts()), PathKind::Code);
    }

    #[test]
    fn classify_skips_vendored_binary_and_lockfiles() {
        assert_eq!(
            classify_path("node_modules/x/index.js", &exts()),
            PathKind::Skip
        );
        assert_eq!(
            classify_path("target/debug/foo.rs", &exts()),
            PathKind::Skip
        );
        assert_eq!(classify_path("vendor/lib/a.go", &exts()), PathKind::Skip);
        assert_eq!(classify_path("logo.png", &exts()), PathKind::Skip);
        assert_eq!(classify_path("assets/font.woff2", &exts()), PathKind::Skip);
        assert_eq!(classify_path("Cargo.lock", &exts()), PathKind::Skip);
        assert_eq!(classify_path("pnpm-lock.yaml", &exts()), PathKind::Skip);
        // An unknown extension is skipped.
        assert_eq!(classify_path("data/blob.xyz", &exts()), PathKind::Skip);
        // A file with no extension and not a README is skipped.
        assert_eq!(classify_path("Makefile", &exts()), PathKind::Skip);
    }

    #[test]
    fn lockfile_under_subdir_is_skipped() {
        assert_eq!(classify_path("sub/yarn.lock", &exts()), PathKind::Skip);
    }

    #[test]
    fn lang_label_derivation() {
        assert_eq!(lang_for_path("a/b.rs"), Some("rust"));
        assert_eq!(lang_for_path("x.tsx"), Some("typescript"));
        assert_eq!(lang_for_path("y.py"), Some("python"));
        assert_eq!(lang_for_path("z.unknownext"), None);
    }

    // ---- issue → RawDocument shaping (pure, offline) ----------------------

    #[test]
    fn shape_issue_concatenates_title_body_and_comments() {
        let item = serde_json::json!({
            "number": 42,
            "title": "Login is broken",
            "body": "Steps: click login, see error.",
            "state": "open",
            "updated_at": "2026-06-01T00:00:00Z",
            "labels": [ { "name": "bug" }, { "name": "auth" } ],
        });
        let doc = shape_issue_document(
            &item,
            42,
            "https://github.com/acme/app/issues/42",
            "issue",
            "acme/app",
            &["First comment".to_string(), "Second comment".to_string()],
        );
        assert_eq!(doc.id, "acme/app#issue-42");
        assert_eq!(doc.source, "https://github.com/acme/app/issues/42");
        assert_eq!(doc.metadata.get("kind").map(String::as_str), Some("issue"));
        assert_eq!(doc.metadata.get("state").map(String::as_str), Some("open"));
        assert_eq!(doc.metadata.get("number").map(String::as_str), Some("42"));
        assert_eq!(
            doc.metadata.get("updated_at").map(String::as_str),
            Some("2026-06-01T00:00:00Z")
        );
        let labels = doc.metadata.get("labels").map(String::as_str).unwrap_or("");
        assert!(
            labels.contains("bug") && labels.contains("auth"),
            "labels: {labels}"
        );
        assert!(doc.content.contains("Login is broken"));
        assert!(doc.content.contains("Steps: click login"));
        assert!(doc.content.contains("Comment 1: First comment"));
        assert!(doc.content.contains("Comment 2: Second comment"));
        assert!(doc.title.as_deref().unwrap().starts_with("issue #42:"));
    }

    #[test]
    fn shape_issue_handles_missing_optional_fields() {
        let item = serde_json::json!({ "number": 1, "title": "T" });
        let doc = shape_issue_document(&item, 1, "u", "issue", "o/r", &[]);
        assert!(doc.content.starts_with('T'));
        assert!(
            !doc.metadata.contains_key("labels"),
            "no labels key when none present"
        );
        assert!(!doc.metadata.contains_key("updated_at"));
    }

    // ---- ACL derivation (pure, offline) -----------------------------------

    #[test]
    fn public_repo_has_no_acl() {
        let cfg = GithubConnectorConfig::new("o", "r", GithubAuth::Unauthenticated)
            .visibility(GithubVisibility::Public);
        assert!(cfg.acl().is_none());
    }

    #[test]
    fn private_repo_uses_configured_groups() {
        let cfg = GithubConnectorConfig::new("o", "r", GithubAuth::Unauthenticated)
            .visibility(GithubVisibility::Private)
            .acl_groups(["eng", "sre"]);
        assert_eq!(cfg.acl(), Some(vec!["eng".to_string(), "sre".to_string()]));
    }

    #[test]
    fn private_repo_without_groups_falls_back_to_repo_scope() {
        let cfg = GithubConnectorConfig::new("acme", "app", GithubAuth::Unauthenticated)
            .visibility(GithubVisibility::Private);
        assert_eq!(cfg.acl(), Some(vec!["github:acme/app".to_string()]));
    }

    #[test]
    fn auth_debug_never_leaks_secrets() {
        let token = GithubAuth::Token("ghp_supersecret".to_string());
        assert!(!format!("{token:?}").contains("supersecret"));
        let app = GithubAuth::AppInstallation {
            app_id: 1,
            private_key: "-----BEGIN RSA PRIVATE KEY-----secret".to_string(),
            installation_id: 2,
        };
        let dbg = format!("{app:?}");
        assert!(!dbg.contains("secret"), "private key leaked: {dbg}");
        assert!(dbg.contains("app_id"));
    }
}
