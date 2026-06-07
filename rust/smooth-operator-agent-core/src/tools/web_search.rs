//! The `web_search` tool — a pluggable web-search seam.
//!
//! Web search needs a paid/external provider (Brave, Bing, Tavily, SerpAPI, …),
//! and we deliberately do NOT hardcode one. Instead this module defines a
//! [`WebSearchProvider`] trait. The default [`NoopWebSearchProvider`] returns a
//! single explanatory result ("no provider configured") so the `web_search`
//! tool is always present and never silently empty — a deployment that wants
//! real search implements the trait and injects it via
//! [`ToolContext::with_web_search`](crate::tools::ToolContext::with_web_search).
//!
//! See `docs/TOOLS.md` for an example custom provider.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use smooth_operator::tool::ToolSchema;
use smooth_operator::Tool;

/// Default number of results requested when the caller doesn't specify `k`.
const DEFAULT_RESULTS: usize = 5;

/// A single web-search hit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// The result title.
    pub title: String,
    /// The result URL.
    pub url: String,
    /// A short snippet / summary of the result.
    pub snippet: String,
}

impl SearchResult {
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

/// A pluggable web-search backend.
///
/// Implement this over a provider's API (Brave/Bing/Tavily/…), then inject it
/// with [`ToolContext::with_web_search`](crate::tools::ToolContext::with_web_search).
/// `search` returns up to `k` results for `query`.
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    /// Run a web search for `query`, returning up to `k` results.
    ///
    /// # Errors
    /// Returns an error if the upstream search call fails.
    async fn search(&self, query: &str, k: usize) -> anyhow::Result<Vec<SearchResult>>;

    /// Human-readable provider name, surfaced in diagnostics. Defaults to
    /// `"unknown"`.
    fn name(&self) -> &str {
        "unknown"
    }
}

/// The default no-op provider: returns a single explanatory result instead of
/// real search hits, so the agent gets a clear "search is unavailable" signal
/// rather than an empty list it might mistake for "no results found".
pub struct NoopWebSearchProvider;

#[async_trait]
impl WebSearchProvider for NoopWebSearchProvider {
    async fn search(&self, query: &str, _k: usize) -> anyhow::Result<Vec<SearchResult>> {
        Ok(vec![SearchResult::new(
            "Web search is not configured",
            "",
            format!(
                "No web-search provider is configured for this agent, so the query {query:?} \
                 could not be run against the live web. To enable web search, implement \
                 WebSearchProvider and inject it via ToolContext::with_web_search."
            ),
        )])
    }

    fn name(&self) -> &str {
        "noop"
    }
}

/// A [`Tool`] that runs a web search through the injected [`WebSearchProvider`].
pub struct WebSearchTool {
    provider: Arc<dyn WebSearchProvider>,
}

impl WebSearchTool {
    /// Build the tool over a provider handle.
    #[must_use]
    pub fn new(provider: Arc<dyn WebSearchProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_search".to_string(),
            description: "Search the public web for up-to-date information not in the \
                          organization's knowledge base (current events, external facts, general \
                          reference). Returns a list of results with title, URL, and snippet. \
                          Prefer knowledge_search for organization-specific questions."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The web-search query."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default 5).",
                        "minimum": 1,
                        "maximum": 10
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
            .ok_or_else(|| anyhow::anyhow!("web_search requires a string 'query' argument"))?;

        let k = arguments
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_RESULTS, |n| (n as usize).clamp(1, 10));

        let results = self.provider.search(query, k).await?;

        if results.is_empty() {
            return Ok(format!("No web-search results found for query: {query:?}"));
        }

        let mut out = format!(
            "Found {} web-search result(s) for {query:?} (provider: {}):\n",
            results.len(),
            self.provider.name()
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

    #[tokio::test]
    async fn noop_provider_returns_explanatory_result() {
        let provider = NoopWebSearchProvider;
        let results = provider.search("latest rust release", 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].title.contains("not configured"));
        assert!(results[0].snippet.contains("latest rust release"));
        assert!(results[0].snippet.contains("WebSearchProvider"));
    }

    #[tokio::test]
    async fn tool_over_noop_provider_explains_missing_provider() {
        let tool = WebSearchTool::new(Arc::new(NoopWebSearchProvider));
        let out = tool
            .execute(serde_json::json!({ "query": "current weather" }))
            .await
            .expect("execute");
        assert!(out.contains("provider: noop"), "got: {out}");
        assert!(out.contains("not configured"), "got: {out}");
        assert!(tool.is_read_only());
    }

    #[tokio::test]
    async fn tool_requires_query() {
        let tool = WebSearchTool::new(Arc::new(NoopWebSearchProvider));
        let err = tool
            .execute(serde_json::json!({ "limit": 3 }))
            .await
            .expect_err("missing query should error");
        assert!(err.to_string().contains("query"));
    }

    /// A custom provider returning real-looking hits, proving the seam: the tool
    /// renders whatever the injected provider returns.
    struct StubProvider;

    #[async_trait]
    impl WebSearchProvider for StubProvider {
        async fn search(&self, query: &str, k: usize) -> anyhow::Result<Vec<SearchResult>> {
            Ok((0..k)
                .map(|i| {
                    SearchResult::new(
                        format!("Result {i} for {query}"),
                        format!("https://example.com/{i}"),
                        format!("snippet {i}"),
                    )
                })
                .collect())
        }
        fn name(&self) -> &str {
            "stub"
        }
    }

    #[tokio::test]
    async fn tool_renders_injected_provider_results() {
        let tool = WebSearchTool::new(Arc::new(StubProvider));
        let out = tool
            .execute(serde_json::json!({ "query": "rust", "limit": 2 }))
            .await
            .expect("execute");
        assert!(out.contains("provider: stub"), "got: {out}");
        assert!(out.contains("Result 0 for rust"), "got: {out}");
        assert!(out.contains("Result 1 for rust"), "got: {out}");
        assert!(out.contains("https://example.com/1"), "got: {out}");
    }
}
