//! The `knowledge_search` tool — the agent's hands on the RAG knowledge base.
//!
//! This is the tool half of the knowledge-grounded turn. While
//! [`AgentConfig::with_knowledge`](smooth_operator_core::AgentConfig::with_knowledge)
//! lets the engine *auto-inject* a few top results as context before the first
//! LLM call, a real agent also wants to **decide** to search — to issue its own
//! query, with its own phrasing, mid-turn. That's what this tool exposes: a
//! `knowledge_search({ "query": "…" })` call the model can emit, which queries
//! the [`StorageAdapter`](crate::adapter::StorageAdapter)'s
//! [`KnowledgeBase`](smooth_operator_core::KnowledgeBase) and returns the top-K
//! matches as text the model reads on the next turn.
//!
//! Construct it from the same `Arc<dyn KnowledgeBase>` the runtime hands
//! `AgentConfig::with_knowledge`, so both paths read the same store.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use smooth_operator_core::tool::ToolSchema;
use smooth_operator_core::{KnowledgeBase, KnowledgeResult, Tool};

use crate::access_control::{AccessContext, AclKnowledgeStore};
use crate::rerank::{apply_optional_rerank, Reranker};

/// A shared sink the [`KnowledgeSearchTool`] records its structured results into,
/// so the runtime can collect the sources a turn's `knowledge_search` calls
/// actually surfaced (for citations) without re-parsing the tool's text output.
pub type KnowledgeResultSink = Arc<Mutex<Vec<KnowledgeResult>>>;

/// Default number of results returned when the caller doesn't specify `limit`.
const DEFAULT_LIMIT: usize = 3;

/// Overfetch multiplier when a reranker is configured.
///
/// A reranker can only promote what it's given, so we pull a wider candidate set
/// from the (cheaper, rank-based) knowledge query and let the reranker pick the
/// final top-K. With no reranker this multiplier is unused — we fetch exactly
/// `limit`, so default behavior is byte-for-byte unchanged.
const RERANK_OVERFETCH: usize = 4;

/// A [`Tool`] that searches the agent's knowledge base.
///
/// Holds an `Arc<dyn KnowledgeBase>` — the exact handle returned by
/// [`StorageAdapter::knowledge`](crate::adapter::StorageAdapter::knowledge) —
/// so a tool call hits the same store the engine auto-injects from.
///
/// Optionally holds an `Arc<dyn Reranker>`: when set, the tool overfetches
/// candidates from the knowledge query and reorders them with the reranker
/// before returning the top-K (Onyx-gap G8). When unset (the default), behavior
/// is unchanged — the knowledge query's own top-`limit` is returned as-is.
pub struct KnowledgeSearchTool {
    knowledge: Arc<dyn KnowledgeBase>,
    reranker: Option<Arc<dyn Reranker>>,
    /// Optional sink the tool records the structured results of every search
    /// into. When set (via [`with_result_sink`](Self::with_result_sink)), the
    /// runtime reads it after the turn to build citations from the documents the
    /// agent's `knowledge_search` calls surfaced. `None` ⇒ no recording
    /// (default), so existing behavior is byte-for-byte unchanged.
    result_sink: Option<KnowledgeResultSink>,
}

impl KnowledgeSearchTool {
    /// Build the tool over a knowledge base handle.
    ///
    /// The handle may itself be an ACL-filtering reader (e.g. from
    /// [`AclKnowledgeStore::reader`](crate::access_control::AclKnowledgeStore::reader)),
    /// in which case the tool's searches are document-level access-controlled.
    /// Use [`with_access_control`](Self::with_access_control) to build that
    /// reader from a store + requester in one step.
    ///
    /// No reranker is configured by default; add one with
    /// [`with_reranker`](Self::with_reranker).
    #[must_use]
    pub fn new(knowledge: Arc<dyn KnowledgeBase>) -> Self {
        Self {
            knowledge,
            reranker: None,
            result_sink: None,
        }
    }

    /// Build the tool bound to a requester's [`AccessContext`] over an
    /// [`AclKnowledgeStore`] (Onyx-gap G3): every search reads through an
    /// ACL-filtering reader, so results the requester is not entitled to are
    /// dropped before they reach the model.
    #[must_use]
    pub fn with_access_control(store: &AclKnowledgeStore, context: AccessContext) -> Self {
        Self {
            knowledge: store.reader(context),
            reranker: None,
            result_sink: None,
        }
    }

    /// Record the structured results of every search into `sink` (Onyx-gap:
    /// structured citations). The runtime drains the sink after a turn to build
    /// the `eventual_response`'s `citations` from the documents the agent's
    /// `knowledge_search` calls actually surfaced. Leaving it unset keeps the
    /// tool's behavior unchanged.
    #[must_use]
    pub fn with_result_sink(mut self, sink: KnowledgeResultSink) -> Self {
        self.result_sink = Some(sink);
        self
    }

    /// Attach an optional reranker stage (Onyx-gap G8).
    ///
    /// When set, the tool overfetches candidates and reorders the top-K with the
    /// [`Reranker`] before returning. Pass a [`LexicalReranker`](crate::rerank::LexicalReranker)
    /// for a deterministic offline reorder, or an adapter-side `GatewayReranker`
    /// for a paid cross-encoder. Leaving it unset keeps default behavior.
    #[must_use]
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = Some(reranker);
        self
    }
}

#[async_trait]
impl Tool for KnowledgeSearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "knowledge_search".to_string(),
            description: "Search the organization's knowledge base for facts relevant to the user's \
                          question (policies, product details, documentation). Returns the most \
                          relevant snippets with their source and relevance score. Call this before \
                          answering any question that depends on organization-specific knowledge."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query — phrase it with the key terms you expect to \
                                        appear in the answer (e.g. 'return policy refund window')."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of snippets to return (default 3).",
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
            .ok_or_else(|| {
                anyhow::anyhow!("knowledge_search requires a string 'query' argument")
            })?;

        let limit = arguments
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_LIMIT, |n| (n as usize).clamp(1, 10));

        // When a reranker is configured, overfetch a wider candidate set so the
        // reranker has room to promote a lexically/semantically better doc that
        // the rank-based query placed lower. With no reranker, fetch exactly
        // `limit` so behavior is unchanged.
        let fetch = if self.reranker.is_some() {
            limit.saturating_mul(RERANK_OVERFETCH)
        } else {
            limit
        };

        // `KnowledgeBase::query` is synchronous in smooth-operator; the in-memory
        // backend is a CPU-bound keyword scan, so calling it directly here is
        // fine (no blocking I/O to offload to a worker thread).
        let candidates = self.knowledge.query(query, fetch)?;

        // Opt-in rerank stage (Onyx-gap G8): reorder + truncate to `limit`. With
        // `None` this is just a truncation, preserving the query's own order.
        let results = apply_optional_rerank(self.reranker.as_ref(), query, candidates, limit).await;

        // Record the structured results so the runtime can build citations from
        // the sources this search surfaced. Done before the empty-check so an
        // empty search records nothing (no spurious citation).
        if let Some(sink) = &self.result_sink {
            if let Ok(mut guard) = sink.lock() {
                guard.extend(results.iter().cloned());
            }
        }

        if results.is_empty() {
            return Ok(format!(
                "No knowledge base results found for query: {query:?}"
            ));
        }

        let mut out = format!(
            "Found {} knowledge base result(s) for {query:?}:\n",
            results.len()
        );
        for (i, result) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. [source={} | id={} | relevance={:.2}]\n{}\n",
                i + 1,
                result.source,
                result.document_id,
                result.score,
                result.chunk,
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
    use smooth_operator_core::{Document, DocumentType, InMemoryKnowledge};

    fn seeded_kb() -> Arc<dyn KnowledgeBase> {
        let kb = InMemoryKnowledge::new();
        kb.ingest(Document::new(
            "SmooAI returns are accepted within 30 days of delivery for a full refund.",
            "policies/returns.md",
            DocumentType::Documentation,
        ))
        .expect("ingest returns policy");
        kb.ingest(Document::new(
            "Standard shipping takes 5 to 7 business days.",
            "policies/shipping.md",
            DocumentType::Documentation,
        ))
        .expect("ingest shipping policy");
        Arc::new(kb)
    }

    #[tokio::test]
    async fn schema_exposes_query_parameter() {
        let tool = KnowledgeSearchTool::new(Arc::new(InMemoryKnowledge::new()));
        let schema = tool.schema();
        assert_eq!(schema.name, "knowledge_search");
        assert_eq!(schema.parameters["properties"]["query"]["type"], "string");
        assert_eq!(schema.parameters["required"][0], "query");
        assert!(tool.is_read_only());
    }

    #[tokio::test]
    async fn execute_returns_matching_document() {
        let tool = KnowledgeSearchTool::new(seeded_kb());
        let out = tool
            .execute(serde_json::json!({ "query": "return policy refund" }))
            .await
            .expect("execute");
        assert!(out.contains("30 days"), "expected returns fact, got: {out}");
        assert!(
            out.contains("policies/returns.md"),
            "expected source, got: {out}"
        );
    }

    #[tokio::test]
    async fn execute_no_match_reports_empty() {
        let tool = KnowledgeSearchTool::new(seeded_kb());
        let out = tool
            .execute(serde_json::json!({ "query": "warranty electronics voltage" }))
            .await
            .expect("execute");
        assert!(out.contains("No knowledge base results"), "got: {out}");
    }

    #[tokio::test]
    async fn execute_rejects_missing_query() {
        let tool = KnowledgeSearchTool::new(seeded_kb());
        let err = tool
            .execute(serde_json::json!({ "limit": 3 }))
            .await
            .expect_err("missing query should error");
        assert!(err.to_string().contains("query"));
    }

    /// The reranker is opt-in: a tool with no reranker returns the knowledge
    /// query's own results unchanged.
    #[tokio::test]
    async fn execute_without_reranker_is_unchanged() {
        let tool = KnowledgeSearchTool::new(seeded_kb());
        assert!(tool.reranker.is_none());
        let out = tool
            .execute(serde_json::json!({ "query": "return policy refund" }))
            .await
            .expect("execute");
        assert!(out.contains("30 days"), "got: {out}");
    }

    /// Wiring smoke test: a tool built `with_reranker` runs the rerank stage and
    /// still returns the relevant result.
    #[tokio::test]
    async fn execute_with_reranker_runs_and_returns_results() {
        use crate::rerank::LexicalReranker;

        let tool =
            KnowledgeSearchTool::new(seeded_kb()).with_reranker(Arc::new(LexicalReranker::new()));
        assert!(tool.reranker.is_some());
        let out = tool
            .execute(serde_json::json!({ "query": "return policy refund", "limit": 1 }))
            .await
            .expect("execute");
        assert!(
            out.contains("30 days") && out.contains("policies/returns.md"),
            "reranked result should still surface the returns fact, got: {out}"
        );
    }
}
