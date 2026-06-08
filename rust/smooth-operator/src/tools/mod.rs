//! Tools the smooth-operator runtime registers on the smooth-operator-core engine.
//!
//! Each tool implements smooth-operator's [`Tool`](smooth_operator_core::Tool) trait
//! so the [`Agent`](smooth_operator_core::Agent) can invoke it during a turn.
//!
//! # Built-in catalog
//!
//! [`builtin_tools`] assembles the default catalog from a [`ToolContext`]:
//!
//! - [`KnowledgeSearchTool`] — RAG search over the organization's knowledge base.
//! - [`ConversationHistoryTool`] — read the current conversation's recent messages.
//! - [`FetchUrlTool`] — fetch a public URL → readable text (SSRF-guarded).
//! - [`WebSearchTool`] — web search through a pluggable [`WebSearchProvider`]
//!   (defaults to [`NoopWebSearchProvider`], which explains that no provider is
//!   configured rather than silently returning nothing).
//!
//! Beyond the default catalog, [`GithubSearchTool`] does live GitHub code/issue
//! search (fresh lookups beyond the indexed snapshot). It needs an explicit
//! [`GithubAuth`] + a default `owner/repo` scope, so a deployment registers it
//! separately (it is not part of [`builtin_tools`]). See `docs/CONNECTORS.md`.
//!
//! See `docs/TOOLS.md` for the tool shape, the catalog, and how to author a
//! custom tool or plug in a web-search provider.

pub mod context;
pub mod conversation_history;
pub mod fetch_url;
pub mod github_search;
pub mod knowledge_search;
pub mod web_search;

pub use context::ToolContext;
pub use conversation_history::ConversationHistoryTool;
pub use fetch_url::FetchUrlTool;
pub use github_search::{
    GithubAuth, GithubSearchBackend, GithubSearchKind, GithubSearchResult, GithubSearchTool,
    OctocrabGithubSearch,
};
pub use knowledge_search::KnowledgeSearchTool;
pub use web_search::{NoopWebSearchProvider, SearchResult, WebSearchProvider, WebSearchTool};

use std::sync::Arc;

use smooth_operator_core::Tool;

/// Assemble the built-in tool catalog from a [`ToolContext`].
///
/// Returns the tools as `Box<dyn Tool>` so the caller registers each on a
/// [`ToolRegistry`](smooth_operator_core::ToolRegistry):
///
/// ```no_run
/// # use std::sync::Arc;
/// # use smooth_operator_core::ToolRegistry;
/// # use smooth_operator::tools::{builtin_tools, ToolContext};
/// # use smooth_operator::adapter::StorageAdapter;
/// # fn wire(storage: Arc<dyn StorageAdapter>) {
/// let ctx = ToolContext::new(storage, "conversation-123");
/// let mut tools = ToolRegistry::new();
/// for tool in builtin_tools(&ctx) {
///     tools.register(tool);
/// }
/// # }
/// ```
///
/// The web-search slot uses whatever provider the context carries — the no-op
/// default unless [`ToolContext::with_web_search`] injected a real one.
#[must_use]
pub fn builtin_tools(ctx: &ToolContext) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(KnowledgeSearchTool::new(ctx.storage.knowledge())),
        Box::new(ConversationHistoryTool::new(
            Arc::clone(&ctx.storage),
            ctx.conversation_id.clone(),
        )),
        Box::new(FetchUrlTool::new()),
        Box::new(WebSearchTool::new(Arc::clone(&ctx.web_search))),
    ]
}

// `builtin_tools` + `conversation_history` behavioral tests that seed the
// in-memory adapter live in `tests/builtin_tools.rs` (integration test) — see
// the note in `conversation_history.rs` for why they can't be `src/` unit
// tests. The pure tools (`fetch_url` SSRF/HTML, `web_search` Noop) keep their
// no-adapter unit tests inline in their modules.
