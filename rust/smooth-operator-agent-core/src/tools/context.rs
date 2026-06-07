//! Shared context for the built-in tool catalog.
//!
//! The knowledge-grounded tools ([`KnowledgeSearchTool`](crate::tools::KnowledgeSearchTool))
//! only need an `Arc<dyn KnowledgeBase>`, but the broader built-in catalog
//! ([`builtin_tools`](crate::tools::builtin_tools)) needs more: the storage
//! adapter (for `conversation_history`), the current conversation id, and an
//! optional pluggable web-search provider. [`ToolContext`] bundles those so
//! `builtin_tools(ctx)` can assemble the whole catalog from one value.

use std::sync::Arc;

use crate::adapter::StorageAdapter;
use crate::tools::web_search::{NoopWebSearchProvider, WebSearchProvider};

/// The context the built-in tool catalog is assembled from.
///
/// Cheap to clone — every field is `Arc`'d or a small owned value. Build one
/// with [`ToolContext::new`] and refine it with the `with_*` setters, then hand
/// it to [`builtin_tools`](crate::tools::builtin_tools).
#[derive(Clone)]
pub struct ToolContext {
    /// The storage adapter — `conversation_history` reads the message log from
    /// it.
    pub storage: Arc<dyn StorageAdapter>,
    /// The conversation the tools operate within. `conversation_history` reads
    /// this conversation's recent messages.
    pub conversation_id: String,
    /// The web-search provider. Defaults to [`NoopWebSearchProvider`], which
    /// returns an explanatory "no provider configured" result — so the
    /// `web_search` tool is always present and never silently a no-op.
    pub web_search: Arc<dyn WebSearchProvider>,
}

impl ToolContext {
    /// Build a context over a storage adapter and conversation id, with the
    /// no-op web-search provider as the default.
    #[must_use]
    pub fn new(storage: Arc<dyn StorageAdapter>, conversation_id: impl Into<String>) -> Self {
        Self {
            storage,
            conversation_id: conversation_id.into(),
            web_search: Arc::new(NoopWebSearchProvider),
        }
    }

    /// Plug in a real web-search provider (e.g. a Brave/Bing/Tavily-backed
    /// implementation). Replaces the default [`NoopWebSearchProvider`].
    #[must_use]
    pub fn with_web_search(mut self, provider: Arc<dyn WebSearchProvider>) -> Self {
        self.web_search = provider;
        self
    }
}
