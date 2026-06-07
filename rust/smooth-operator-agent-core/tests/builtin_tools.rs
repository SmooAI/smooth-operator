//! Integration tests for the built-in tool catalog that need the in-memory
//! storage adapter (the conformance baseline). No network is touched.
//!
//! These live here rather than as `src/` unit tests because the in-memory
//! adapter is a dev-dependency that itself links `smooth-operator-agent-core`;
//! an integration test links the lib exactly once, so the `StorageAdapter`
//! trait impls line up (a `src/` unit test would see two copies of the crate).

use std::sync::Arc;

use smooth_operator::Tool;
use smooth_operator_agent_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_agent_core::domain::{Direction, Message as DomainMessage, MessageContent};
use smooth_operator_agent_core::tools::{builtin_tools, ConversationHistoryTool, ToolContext};
use smooth_operator_agent_core::StorageAdapter;

fn msg(conv: &str, direction: Direction, text: &str) -> DomainMessage {
    DomainMessage {
        id: uuid::Uuid::new_v4().to_string(),
        external_id: None,
        organization_id: None,
        conversation_id: Some(conv.to_string()),
        direction,
        content: MessageContent::from_text(text),
        from: None,
        to: None,
        metadata_json: None,
        analytics_json: None,
        created_at: chrono::Utc::now(),
        updated_at: None,
    }
}

async fn seeded(conv: &str) -> Arc<InMemoryStorageAdapter> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    storage
        .append_message(msg(conv, Direction::Inbound, "My name is Zog."))
        .await
        .unwrap();
    storage
        .append_message(msg(conv, Direction::Outbound, "Hi Zog!"))
        .await
        .unwrap();
    storage
        .append_message(msg(conv, Direction::Inbound, "What is my name?"))
        .await
        .unwrap();
    storage
}

// ---- conversation_history -----------------------------------------------

#[tokio::test]
async fn conversation_history_returns_seeded_messages_oldest_first() {
    let storage = seeded("conv-hist").await;
    let tool = ConversationHistoryTool::new(storage, "conv-hist");
    let out = tool.execute(serde_json::json!({})).await.expect("execute");

    assert!(out.contains("3 message(s)"), "got: {out}");
    assert!(out.contains("- User: My name is Zog."), "got: {out}");
    assert!(out.contains("- Agent: Hi Zog!"), "got: {out}");
    assert!(out.contains("- User: What is my name?"), "got: {out}");

    let first = out.find("My name is Zog.").unwrap();
    let last = out.find("What is my name?").unwrap();
    assert!(first < last, "expected oldest-first order, got: {out}");
}

#[tokio::test]
async fn conversation_history_limit_caps_to_most_recent() {
    let storage = seeded("conv-hist-2").await;
    let tool = ConversationHistoryTool::new(storage, "conv-hist-2");
    let out = tool
        .execute(serde_json::json!({ "limit": 1 }))
        .await
        .expect("execute");
    assert!(out.contains("1 message(s)"), "got: {out}");
    assert!(out.contains("What is my name?"), "got: {out}");
    assert!(!out.contains("My name is Zog."), "got: {out}");
}

#[tokio::test]
async fn conversation_history_empty_reports_no_messages() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let tool = ConversationHistoryTool::new(storage, "nope");
    let out = tool.execute(serde_json::json!({})).await.expect("execute");
    assert!(out.contains("No messages"), "got: {out}");
    assert!(tool.is_read_only());
}

// ---- builtin_tools catalog ----------------------------------------------

#[test]
fn builtin_catalog_assembles_all_four_tools() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let ctx = ToolContext::new(storage, "conv-cat");
    let tools = builtin_tools(&ctx);

    let names: Vec<String> = tools.iter().map(|t| t.schema().name).collect();
    assert_eq!(
        names.len(),
        4,
        "catalog should have 4 tools, got: {names:?}"
    );
    assert!(names.contains(&"knowledge_search".to_string()));
    assert!(names.contains(&"conversation_history".to_string()));
    assert!(names.contains(&"fetch_url".to_string()));
    assert!(names.contains(&"web_search".to_string()));
}

#[test]
fn all_builtin_tools_are_read_only() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let ctx = ToolContext::new(storage, "conv-ro");
    for tool in builtin_tools(&ctx) {
        assert!(
            tool.is_read_only(),
            "{} should be read-only",
            tool.schema().name
        );
    }
}

#[tokio::test]
async fn builtin_web_search_explains_missing_provider() {
    // With no provider injected, the catalog's web_search uses the Noop default
    // and returns an explanatory result rather than silently empty.
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let ctx = ToolContext::new(storage, "conv-ws");
    let tools = builtin_tools(&ctx);
    let web = tools
        .iter()
        .find(|t| t.schema().name == "web_search")
        .expect("web_search in catalog");
    let out = web
        .execute(serde_json::json!({ "query": "anything" }))
        .await
        .expect("execute");
    assert!(out.contains("not configured"), "got: {out}");
}
