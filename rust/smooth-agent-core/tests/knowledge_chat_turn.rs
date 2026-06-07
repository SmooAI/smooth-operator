//! End-to-end knowledge-chat turn — driven entirely by a MockLlmClient, so it
//! runs with **no API keys and no network**.
//!
//! This is the heart of the "onyx-like" value: a user asks a question, the
//! agent searches the knowledge base, the retrieved fact comes back, and the
//! agent answers grounded in it — all through the real smooth-operator agent
//! loop (`Agent::run`), with the only fake being the LLM's scripted decisions.

use std::sync::Arc;

use smooth_agent_adapter_memory::InMemoryStorageAdapter;
use smooth_agent_core::runtime::KnowledgeChatRuntime;
use smooth_agent_core::{Direction, MessageQuery, StorageAdapter};
use smooth_operator::llm_provider::{LlmProvider, MockLlmClient};
use smooth_operator::{Document, DocumentType, LlmConfig};

/// Build an in-memory adapter and seed its knowledge base with a few support
/// documents.
fn seeded_storage() -> Arc<InMemoryStorageAdapter> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let kb = storage.knowledge();
    kb.ingest(Document::new(
        "SmooAI returns are accepted within 30 days of delivery for a full refund.",
        "policies/returns.md",
        DocumentType::Documentation,
    ))
    .expect("ingest returns policy");
    kb.ingest(Document::new(
        "Standard shipping takes 5 to 7 business days within the continental US.",
        "policies/shipping.md",
        DocumentType::Documentation,
    ))
    .expect("ingest shipping policy");
    kb.ingest(Document::new(
        "Premium support is available 24/7 by phone and chat for Pro-tier customers.",
        "policies/support.md",
        DocumentType::Documentation,
    ))
    .expect("ingest support policy");
    storage
}

fn test_llm() -> LlmConfig {
    // Never used to make a real call — the mock provider intercepts every
    // request — but the runtime requires a config to construct the agent.
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// The headline test: a knowledge-grounded turn where the agent decides to
/// search, gets the seeded fact back, and answers using it.
#[tokio::test]
async fn knowledge_grounded_turn_searches_then_answers() {
    let storage = seeded_storage();

    // ----- Script the mock LLM -------------------------------------------
    // Turn 1: the model emits a tool_call for knowledge_search.
    // Turn 2 (after the tool result lands): the model emits the final answer
    //         that references the retrieved 30-day fact.
    let mock = MockLlmClient::new();
    mock.push_tool_call(
        "call_kb_1",
        "knowledge_search",
        serde_json::json!({ "query": "return policy refund window" }),
    )
    .push_text(
        "Our return policy: items are accepted within 30 days of delivery for a full refund.",
    );

    let runtime = KnowledgeChatRuntime::new(storage.clone(), test_llm())
        .with_llm_provider(Arc::new(mock.clone()));

    let outcome = runtime
        .run_turn("conv-1", "What is the return policy?")
        .await
        .expect("run_turn");

    // (a) The knowledge_search tool was actually invoked by the agent.
    assert!(
        outcome.invoked_tool("knowledge_search"),
        "expected the agent to call knowledge_search; events: {:?}",
        outcome.events
    );

    // (b) The tool returned the seeded document (the 30-day return fact +
    //     its source), proving retrieval ran against the real KB.
    let tool_result = outcome
        .tool_result("knowledge_search")
        .expect("knowledge_search should have a completed result");
    assert!(
        tool_result.contains("30 days"),
        "tool result should contain the seeded fact, got: {tool_result}"
    );
    assert!(
        tool_result.contains("policies/returns.md"),
        "tool result should cite the seeded source, got: {tool_result}"
    );

    // (c) The final response is the mock's scripted, grounded answer.
    assert_eq!(
        outcome.reply,
        "Our return policy: items are accepted within 30 days of delivery for a full refund."
    );

    // (d) No real/network LLM call happened — exactly two scripted mock
    //     calls were made (the tool-call turn + the final-answer turn), and
    //     both were recorded by the mock rather than going to a live model.
    assert_eq!(
        mock.call_count(),
        2,
        "expected exactly 2 mock LLM calls (search decision + answer)"
    );

    // Bonus: the auto-injected knowledge context reached the model. The first
    // recorded call's messages include the engine's `[Relevant knowledge]`
    // system injection built from `AgentConfig::with_knowledge`.
    let first_call = &mock.calls()[0];
    let injected_knowledge = first_call
        .messages
        .iter()
        .any(|m| m.content.contains("[Relevant knowledge]") && m.content.contains("30 days"));
    assert!(
        injected_knowledge,
        "expected with_knowledge to auto-inject the retrieved fact into the first LLM request"
    );

    // The mock saw the knowledge_search tool schema offered to it on turn 1.
    assert!(
        first_call
            .tools
            .iter()
            .any(|t| t.name == "knowledge_search"),
        "knowledge_search tool schema should be offered to the model"
    );

    // The turn persisted both the inbound question and the outbound answer.
    let page = storage
        .list_messages_by_conversation(MessageQuery::new("conv-1", 10))
        .await
        .expect("list messages");
    assert_eq!(
        page.messages.len(),
        2,
        "inbound + outbound should be persisted"
    );
    assert_eq!(page.messages[0].direction, Direction::Inbound);
    assert_eq!(
        page.messages[0].content.text.as_deref(),
        Some("What is the return policy?")
    );
    assert_eq!(page.messages[1].direction, Direction::Outbound);
    assert_eq!(
        page.messages[1].content.text.as_deref(),
        Some("Our return policy: items are accepted within 30 days of delivery for a full refund.")
    );
}

/// A simpler no-tool turn: the model answers directly with text, no
/// knowledge_search. Proves the runtime returns cleanly without a tool call.
#[tokio::test]
async fn no_tool_turn_returns_text_cleanly() {
    let storage = seeded_storage();

    let mock = MockLlmClient::new();
    mock.push_text("Hi! How can I help you today?");

    let runtime = KnowledgeChatRuntime::new(storage.clone(), test_llm())
        .with_llm_provider(Arc::new(mock.clone()));

    let outcome = runtime.run_turn("conv-2", "hello").await.expect("run_turn");

    assert_eq!(outcome.reply, "Hi! How can I help you today?");
    assert!(
        !outcome.invoked_tool("knowledge_search"),
        "no tool should be invoked on a plain greeting"
    );
    assert_eq!(
        mock.call_count(),
        1,
        "exactly one LLM call, no tool round-trip"
    );

    // Both messages still persisted.
    let page = storage
        .list_messages_by_conversation(MessageQuery::new("conv-2", 10))
        .await
        .expect("list messages");
    assert_eq!(page.messages.len(), 2);
}

/// The mock is usable purely as a trait object, mirroring how production wires
/// a real `LlmClient` — a small belt-and-suspenders that the injection seam
/// accepts any `LlmProvider`.
#[tokio::test]
async fn runtime_accepts_any_llm_provider_trait_object() {
    let storage = seeded_storage();
    let provider: Arc<dyn LlmProvider> = Arc::new(MockLlmClient::new());
    let runtime = KnowledgeChatRuntime::new(storage, test_llm()).with_llm_provider(provider);
    // Empty script => mock returns a benign empty terminal response.
    let outcome = runtime
        .run_turn("conv-3", "anything")
        .await
        .expect("run_turn");
    assert!(outcome.reply.is_empty());
}
