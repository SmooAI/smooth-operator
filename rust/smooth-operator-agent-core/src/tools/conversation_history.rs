//! The `conversation_history` tool — the agent's view of the current
//! conversation's recent messages.
//!
//! `AgentConfig::with_prior_messages` already replays history into the model's
//! context (see `KnowledgeChatRuntime::run_turn`), but an explicit
//! `conversation_history` tool lets the agent *deliberately* re-read the log —
//! e.g. to summarize "what have we discussed?", or to pull an earlier detail it
//! didn't keep in working memory. It reads the same persisted message log
//! through the [`StorageAdapter`], scoped to the current `conversation_id` from
//! the [`ToolContext`](crate::tools::ToolContext).

use std::sync::Arc;

use async_trait::async_trait;

use smooth_operator::tool::ToolSchema;
use smooth_operator::Tool;

use crate::adapter::{MessageQuery, StorageAdapter};
use crate::domain::Direction;

/// Default number of recent messages returned when `limit` is omitted.
const DEFAULT_LIMIT: usize = 20;
/// Hard cap on `limit` regardless of what the model asks for.
const MAX_LIMIT: usize = 100;

/// A [`Tool`] that returns the current conversation's recent messages.
///
/// Bound to a single `conversation_id` (the one in the [`ToolContext`]); the
/// model can only read the conversation it is participating in — it cannot pass
/// an arbitrary conversation id.
pub struct ConversationHistoryTool {
    storage: Arc<dyn StorageAdapter>,
    conversation_id: String,
}

impl ConversationHistoryTool {
    /// Build the tool over a storage adapter and the current conversation id.
    #[must_use]
    pub fn new(storage: Arc<dyn StorageAdapter>, conversation_id: impl Into<String>) -> Self {
        Self {
            storage,
            conversation_id: conversation_id.into(),
        }
    }
}

#[async_trait]
impl Tool for ConversationHistoryTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "conversation_history".to_string(),
            description: "Read the most recent messages of the CURRENT conversation (oldest-first \
                          within the returned window). Use this to recall earlier details the user \
                          mentioned, or to summarize the discussion so far. Returns each message's \
                          direction (user vs agent) and text."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of recent messages to return (default 20).",
                        "minimum": 1,
                        "maximum": 100
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<String> {
        let limit = arguments
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_LIMIT, |n| (n as usize).clamp(1, MAX_LIMIT));

        // Fetch the most-recent `limit` messages (descending), then reverse to
        // oldest-first so the rendered transcript reads top-to-bottom.
        let query = MessageQuery {
            conversation_id: self.conversation_id.clone(),
            limit,
            cursor: None,
            descending: true,
        };
        let mut page = self.storage.list_messages_by_conversation(query).await?;
        page.messages.reverse();

        if page.messages.is_empty() {
            return Ok("No messages in this conversation yet.".to_string());
        }

        let mut out = format!(
            "Recent conversation history ({} message(s), oldest-first):\n",
            page.messages.len()
        );
        for m in &page.messages {
            let speaker = match m.direction {
                Direction::Inbound => "User",
                Direction::Outbound => "Agent",
            };
            let text = m
                .content
                .text
                .clone()
                .or_else(|| m.content.items.iter().find_map(|it| it.text.clone()))
                .unwrap_or_default();
            out.push_str(&format!("- {speaker}: {text}\n"));
        }
        Ok(out)
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

// Behavioral tests that seed the in-memory adapter live in
// `tests/builtin_tools.rs` (an integration test), because they depend on the
// `smooth-operator-agent-adapter-memory` dev-dependency — which itself links
// `smooth-operator-agent-core`. Exercising that from a `src/` unit test would
// pull in two distinct copies of this crate (the lib-under-test + the dep copy)
// and the `StorageAdapter` trait impls wouldn't line up. Integration tests link
// the lib exactly once, so they don't hit that.
