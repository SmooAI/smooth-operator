//! Demo-only built-in tools, registered **only** when the server runs with
//! `SMOOTH_AGENT_SEED_KB=1` (the seeded-demo flavor).
//!
//! The demo exists to show off **write-confirmation HITL** on a genuinely
//! destructive action. Gating a *read* (`knowledge_search`) behind approval is
//! backwards — an approval should guard a *write*. So the demo ships a mock
//! [`IssueRefundTool`]: a write the agent calls after checking the return
//! policy, which `SMOOTH_AGENT_CONFIRM_TOOLS=issue_refund` parks for human
//! approval.
//!
//! It is a **mock**: `execute` performs no side effect and returns a canned
//! success string. Nothing here is registered in the default (non-demo) flavor,
//! so production behavior is byte-for-byte unchanged.

use async_trait::async_trait;
use smooth_operator_core::tool::ToolSchema;
use smooth_operator_core::{Tool, ToolRegistry};

/// Demo customer-support persona used **instead of**
/// [`KNOWLEDGE_CHAT_SYSTEM_PROMPT`](crate::runner) when the server runs in
/// seeded-demo mode and no per-agent/per-org persona overrides it. Adds the
/// refund flow so the demo agent looks up the return policy (a read, no
/// approval) and then calls `issue_refund` (a write, parked for approval).
pub const DEMO_CHAT_SYSTEM_PROMPT: &str =
    "You are a helpful customer-support agent for the organization. \
    Answer the user's question accurately and concisely. When a question depends on \
    organization-specific facts (policies, products, documentation), call the \
    `knowledge_search` tool to retrieve them before answering, and ground your answer \
    in what you retrieve. When the user asks to return an order or get a refund, FIRST \
    call `knowledge_search` to look up the return policy, then call `issue_refund` with \
    their order id to process the refund. If the knowledge base has no relevant \
    information, say so. Remember facts the user tells you within the conversation and \
    use them when asked.";

/// A **mock** write tool: pretends to issue a refund for an order and returns a
/// canned confirmation. No real side effect — it exists so the HITL demo gates
/// approval on a write instead of a read. Marked not read-only so a
/// permission/surveillance hook can tell it is a write.
pub struct IssueRefundTool;

impl IssueRefundTool {
    /// Construct the mock refund tool.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for IssueRefundTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for IssueRefundTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "issue_refund".to_string(),
            description: "Issues a refund for an order. This is a write action and cannot be \
                          undone. Call it only after the customer has confirmed they want a refund \
                          and you have checked the return policy."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "order_id": {
                        "type": "string",
                        "description": "The order to refund (e.g. 'ORD-1234')."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional reason the customer gave for the refund."
                    }
                },
                "required": ["order_id"]
            }),
        }
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<String> {
        // Loose parsing on purpose: the demo never fails on a missing/oddly-typed
        // order id — it just names whatever it was given.
        let order_id = arguments
            .get("order_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("your order");
        Ok(format!(
            "Refund issued for order {order_id}. It will appear in 3–5 business days."
        ))
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

/// Register the demo-only built-in tools into `tools`.
///
/// When `demo` is false (the default flavor) this is a no-op, so the registry is
/// exactly the built-ins and production behavior is byte-for-byte unchanged.
/// When true (seeded-demo mode), the mock [`IssueRefundTool`] is added.
pub fn register_demo_tools(tools: &mut ToolRegistry, demo: bool) {
    if demo {
        tools.register(IssueRefundTool::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn schema_marks_a_write_action() {
        let tool = IssueRefundTool::new();
        let schema = tool.schema();
        assert_eq!(schema.name, "issue_refund");
        assert_eq!(schema.parameters["required"][0], "order_id");
        assert!(
            !tool.is_read_only(),
            "issue_refund must report itself as a write so a permission hook can gate it"
        );
        assert!(
            schema.description.to_lowercase().contains("write action"),
            "description should mark it a write: {}",
            schema.description
        );
    }

    #[tokio::test]
    async fn execute_returns_canned_confirmation_with_the_order_id() {
        let out = IssueRefundTool::new()
            .execute(serde_json::json!({ "order_id": "ORD-1234", "reason": "changed mind" }))
            .await
            .expect("execute");
        assert!(out.contains("ORD-1234"), "should name the order: {out}");
        assert!(
            out.contains("3–5 business days"),
            "should be the canned success: {out}"
        );
    }

    #[tokio::test]
    async fn execute_tolerates_a_missing_order_id() {
        let out = IssueRefundTool::new()
            .execute(serde_json::json!({}))
            .await
            .expect("execute should not fail on a missing order id");
        assert!(out.contains("Refund issued"), "got: {out}");
    }

    /// The gate: the mock write tool is registered only in demo mode, and never
    /// in the default flavor — so production is byte-for-byte unchanged.
    #[test]
    fn register_demo_tools_only_adds_in_demo_mode() {
        let mut off = ToolRegistry::new();
        register_demo_tools(&mut off, false);
        assert!(
            !off.has_tool("issue_refund"),
            "default flavor must not register the demo write tool"
        );

        let mut on = ToolRegistry::new();
        register_demo_tools(&mut on, true);
        assert!(
            on.has_tool("issue_refund"),
            "seeded-demo mode must register the demo write tool"
        );
    }
}
