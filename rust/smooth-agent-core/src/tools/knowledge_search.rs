//! The `knowledge_search` tool — the agent's hands on the RAG knowledge base.
//!
//! This is the tool half of the knowledge-grounded turn. While
//! [`AgentConfig::with_knowledge`](smooth_operator::AgentConfig::with_knowledge)
//! lets the engine *auto-inject* a few top results as context before the first
//! LLM call, a real agent also wants to **decide** to search — to issue its own
//! query, with its own phrasing, mid-turn. That's what this tool exposes: a
//! `knowledge_search({ "query": "…" })` call the model can emit, which queries
//! the [`StorageAdapter`](crate::adapter::StorageAdapter)'s
//! [`KnowledgeBase`](smooth_operator::KnowledgeBase) and returns the top-K
//! matches as text the model reads on the next turn.
//!
//! Construct it from the same `Arc<dyn KnowledgeBase>` the runtime hands
//! `AgentConfig::with_knowledge`, so both paths read the same store.

use std::sync::Arc;

use async_trait::async_trait;
use smooth_operator::tool::ToolSchema;
use smooth_operator::{KnowledgeBase, Tool};

/// Default number of results returned when the caller doesn't specify `limit`.
const DEFAULT_LIMIT: usize = 3;

/// A [`Tool`] that searches the agent's knowledge base.
///
/// Holds an `Arc<dyn KnowledgeBase>` — the exact handle returned by
/// [`StorageAdapter::knowledge`](crate::adapter::StorageAdapter::knowledge) —
/// so a tool call hits the same store the engine auto-injects from.
pub struct KnowledgeSearchTool {
    knowledge: Arc<dyn KnowledgeBase>,
}

impl KnowledgeSearchTool {
    /// Build the tool over a knowledge base handle.
    #[must_use]
    pub fn new(knowledge: Arc<dyn KnowledgeBase>) -> Self {
        Self { knowledge }
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

        // `KnowledgeBase::query` is synchronous in smooth-operator; the in-memory
        // backend is a CPU-bound keyword scan, so calling it directly here is
        // fine (no blocking I/O to offload to a worker thread).
        let results = self.knowledge.query(query, limit)?;

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
    use smooth_operator::{Document, DocumentType, InMemoryKnowledge};

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
}
