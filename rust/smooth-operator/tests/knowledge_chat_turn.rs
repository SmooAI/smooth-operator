//! End-to-end knowledge-chat turn — driven entirely by a MockLlmClient, so it
//! runs with **no API keys and no network**.
//!
//! This is the heart of the "onyx-like" value: a user asks a question, the
//! agent searches the knowledge base, the retrieved fact comes back, and the
//! agent answers grounded in it — all through the real smooth-operator agent
//! loop (`Agent::run`), with the only fake being the LLM's scripted decisions.

use std::sync::Arc;

use smooth_operator::runtime::KnowledgeChatRuntime;
use smooth_operator::{Direction, MessageQuery, StorageAdapter};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm_provider::{LlmProvider, MockLlmClient};
use smooth_operator_core::{Document, DocumentType, LlmConfig};

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

/// Citations (structured): a knowledge-grounded turn attaches the sources that
/// actually grounded the answer to its [`TurnOutcome`]. Seeds two distinctive
/// docs — one with a GitHub-style `http(s)` source (so it carries a citation
/// `url`), one with a local path (no url) — runs a turn that retrieves the
/// GitHub doc via `knowledge_search`, and asserts the citation carries the
/// grounding doc's id + url + a snippet, deduplicated.
#[tokio::test]
async fn grounded_turn_attaches_deduped_citations_with_url_and_snippet() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let kb = storage.knowledge();
    // (1) GitHub-sourced doc: `source` is the blob URL the connector stamps on
    //     at ingest, so its citation must carry that `url`.
    kb.ingest(Document::new(
        "Quokkas are the friendliest marsupial and famously photogenic.",
        "https://github.com/acme/handbook/blob/main/wildlife/quokka.md",
        DocumentType::Documentation,
    ))
    .expect("ingest quokka doc");
    // (2) Local doc with no web source: any citation for it carries no url.
    kb.ingest(Document::new(
        "Standard shipping takes 5 to 7 business days.",
        "policies/shipping.md",
        DocumentType::Documentation,
    ))
    .expect("ingest shipping doc");

    // Script the mock: search for the quokka fact, then answer. The query is
    // distinctive enough that only the quokka doc matches.
    let mock = MockLlmClient::new();
    mock.push_tool_call(
        "call_kb_quokka",
        "knowledge_search",
        serde_json::json!({ "query": "quokka friendliest photogenic marsupial" }),
    )
    .push_text("Quokkas are the friendliest marsupial — and very photogenic!");

    let runtime = KnowledgeChatRuntime::new(storage.clone(), test_llm())
        .with_llm_provider(Arc::new(mock.clone()));

    let outcome = runtime
        .run_turn("conv-cite", "Tell me about quokkas")
        .await
        .expect("run_turn");

    // The grounding doc shows up as a citation exactly once (deduped across the
    // auto-injected context AND the knowledge_search tool result, both of which
    // surfaced the same quokka doc).
    let quokka: Vec<_> = outcome
        .citations
        .iter()
        .filter(|c| c.snippet.contains("Quokkas"))
        .collect();
    assert_eq!(
        quokka.len(),
        1,
        "the quokka doc should be cited exactly once (deduped); citations: {:?}",
        outcome.citations
    );
    let cite = quokka[0];

    // id is the source document's id (non-empty), url is the GitHub blob URL
    // (from the doc's http(s) `source`), and the snippet is the retrieved chunk.
    assert!(!cite.id.is_empty(), "citation must carry a document id");
    assert_eq!(
        cite.url.as_deref(),
        Some("https://github.com/acme/handbook/blob/main/wildlife/quokka.md"),
        "GitHub-sourced doc's citation must carry its blob url"
    );
    assert!(
        cite.snippet.contains("friendliest marsupial"),
        "citation snippet should be the retrieved chunk, got: {:?}",
        cite.snippet
    );
    assert!(cite.score > 0.0, "citation should carry a relevance score");

    // Sanity: the unrelated shipping doc did NOT ground this turn, so it is not
    // cited (only the sources that actually grounded the answer appear).
    assert!(
        outcome
            .citations
            .iter()
            .all(|c| !c.snippet.contains("shipping")),
        "unrelated shipping doc should not be cited; citations: {:?}",
        outcome.citations
    );
}

/// The no-citations case: a turn that retrieves nothing carries no citations.
/// A plain greeting matches no knowledge doc (the engine's auto-injection query
/// returns empty) and the model answers directly without `knowledge_search`.
#[tokio::test]
async fn turn_with_no_retrieval_has_no_citations() {
    let storage = seeded_storage();

    let mock = MockLlmClient::new();
    mock.push_text("Hi! How can I help you today?");

    let runtime = KnowledgeChatRuntime::new(storage.clone(), test_llm())
        .with_llm_provider(Arc::new(mock.clone()));

    // A greeting with no overlap with any seeded policy doc → no retrieval.
    let outcome = runtime
        .run_turn("conv-nocite", "zzqq")
        .await
        .expect("run_turn");

    assert!(
        !outcome.invoked_tool("knowledge_search"),
        "no knowledge_search on a no-match greeting"
    );
    assert!(
        outcome.citations.is_empty(),
        "a turn that retrieves nothing must carry no citations, got: {:?}",
        outcome.citations
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

/// Cross-turn memory (no network): two turns on the same conversation id. The
/// runtime must replay turn 1's persisted messages into turn 2's agent via
/// `with_prior_messages`, so the mock's turn-2 request carries turn 1's user
/// message AND the turn-1 assistant reply as prior context. This is the
/// no-network proof of the same fix the live `multi_turn_coherence` eval
/// exercises end-to-end.
#[tokio::test]
async fn run_turn_replays_prior_messages_for_cross_turn_memory() {
    let storage = seeded_storage();

    // Turn 1: model just acknowledges. Turn 2: model recalls the name. We don't
    // rely on the mock's reply for the assertion — we assert on what the mock
    // SAW on turn 2 (the replayed prior messages).
    let mock = MockLlmClient::new();
    mock.push_text("Got it, Zog.")
        .push_text("Your name is Zog.");

    let runtime = KnowledgeChatRuntime::new(storage.clone(), test_llm())
        .with_llm_provider(Arc::new(mock.clone()));

    runtime
        .run_turn("conv-mem", "My name is Zog. Just acknowledge.")
        .await
        .expect("turn 1");
    let second = runtime
        .run_turn("conv-mem", "What is my name?")
        .await
        .expect("turn 2");

    assert_eq!(second.reply, "Your name is Zog.");

    // Two turns × one mock call each (no tool round-trips) = 2 calls.
    assert_eq!(mock.call_count(), 2, "one LLM call per turn");

    // The turn-2 request must contain turn 1's content as prior messages: the
    // user's "My name is Zog" turn and the assistant's "Got it, Zog." reply,
    // both replayed before the current "What is my name?" turn.
    let calls = mock.calls();
    let turn2_call = &calls[1];
    let saw_prior_user = turn2_call
        .messages
        .iter()
        .any(|m| m.content.contains("My name is Zog"));
    let saw_prior_assistant = turn2_call
        .messages
        .iter()
        .any(|m| m.content.contains("Got it, Zog."));
    assert!(
        saw_prior_user,
        "turn 2 should replay turn 1's user message as prior context; messages: {:?}",
        turn2_call.messages
    );
    assert!(
        saw_prior_assistant,
        "turn 2 should replay turn 1's assistant reply as prior context; messages: {:?}",
        turn2_call.messages
    );

    // Sanity: turn 1's request had NO prior messages about a name (nothing was
    // persisted before it ran).
    let turn1_call = &calls[0];
    let turn1_user_turns = turn1_call
        .messages
        .iter()
        .filter(|m| m.content.contains("My name is Zog"))
        .count();
    assert_eq!(
        turn1_user_turns, 1,
        "turn 1 should see its own user message exactly once (no prior replay)"
    );
}
