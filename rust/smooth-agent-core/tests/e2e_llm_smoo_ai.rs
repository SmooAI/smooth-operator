//! Real-LLM end-to-end tests against the live `llm.smoo.ai` gateway.
//!
//! These tests drive the **actual** smooth-operator engine (via
//! [`KnowledgeChatRuntime`]) and the **actual** [`LlmClient`] against the live
//! OpenAI-compatible LiteLLM proxy at `https://llm.smoo.ai/v1`, using the cheap
//! `claude-haiku-4-5` model. There is no mock here — every assertion is about
//! real model behavior.
//!
//! ## Gating (safe to commit, safe in CI)
//!
//! Each test is a no-op unless BOTH of these are set:
//!   - `SMOOTH_AGENT_E2E=1`           — explicit opt-in flag
//!   - `SMOOAI_GATEWAY_KEY=<key>`     — the gateway API key (never hardcoded)
//!
//! When either is missing the test prints a skip notice and returns early
//! (it does NOT fail). So `cargo test` with no env stays green, and CI without
//! credentials stays green.
//!
//! ## Verifying locally (does not print the key)
//!
//! ```sh
//! export SMOOAI_GATEWAY_KEY=$(python3 -c \
//!   "import json;print(json.load(open('$HOME/.local/share/opencode/auth.json'))['smooai']['key'])")
//! export SMOOTH_AGENT_E2E=1
//! cargo test -p smooai-smooth-agent-core --test e2e_llm_smoo_ai \
//!   -- --nocapture --test-threads=1
//! ```
//!
//! The model is paid-per-token, so prompts are terse and `max_tokens` is low.

use std::sync::Arc;

use smooth_agent_adapter_memory::InMemoryStorageAdapter;
use smooth_agent_core::runtime::KnowledgeChatRuntime;
use smooth_agent_core::StorageAdapter;
use smooth_operator::llm::{ApiFormat, RetryPolicy};
use smooth_operator::{Document, DocumentType, LlmClient, LlmConfig, Message, StreamEvent};

use futures_util::StreamExt;

const GATEWAY_URL: &str = "https://llm.smoo.ai/v1";
const CHEAP_MODEL: &str = "claude-haiku-4-5";

/// Returns the gateway key from the env, or `None` (with a printed skip notice)
/// when the test should be skipped. NEVER prints the key value.
fn gate(test_name: &str) -> Option<String> {
    if std::env::var("SMOOTH_AGENT_E2E").as_deref() != Ok("1") {
        eprintln!("[skip] {test_name}: SMOOTH_AGENT_E2E != \"1\" — skipping live-gateway test");
        return None;
    }
    match std::env::var("SMOOAI_GATEWAY_KEY") {
        Ok(key) if !key.trim().is_empty() => Some(key),
        _ => {
            eprintln!(
                "[skip] {test_name}: SMOOAI_GATEWAY_KEY unset/empty — skipping live-gateway test"
            );
            None
        }
    }
}

/// A minimal `LlmConfig` pointed at the live gateway with the cheap model.
/// `max_tokens` is kept low because this hits a paid endpoint.
fn live_config(api_key: String) -> LlmConfig {
    LlmConfig {
        api_url: GATEWAY_URL.into(),
        api_key,
        model: CHEAP_MODEL.into(),
        max_tokens: 512,
        temperature: 0.0,
        // Tolerate Cloudflare/LiteLLM transient 5xx without making the test flaky.
        retry_policy: RetryPolicy::default(),
        api_format: ApiFormat::OpenAiCompat,
    }
}

/// Build an in-memory adapter and seed its KB with a distinctive return-window
/// fact. The number (17 days) is deliberately unusual so a generic, ungrounded
/// answer can't accidentally match it.
fn seeded_storage() -> Arc<InMemoryStorageAdapter> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let kb = storage.knowledge();
    kb.ingest(Document::new(
        "SmooAI's return window is exactly 17 days from delivery.",
        "policies/returns.md",
        DocumentType::Documentation,
    ))
    .expect("ingest returns policy");
    storage
}

/// 1. **plain_completion** — drive a real turn through `KnowledgeChatRuntime`
///    (no special knowledge) and assert the live model produced a reply that
///    contains "PONG". Proves real inference through the engine.
#[tokio::test]
async fn plain_completion() {
    let Some(key) = gate("plain_completion") else {
        return;
    };

    // Empty KB — this turn exercises plain inference, not retrieval.
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let runtime = KnowledgeChatRuntime::new(storage, live_config(key)).with_max_iterations(4);

    let outcome = runtime
        .run_turn(
            "e2e-plain",
            "Reply with exactly the word PONG and nothing else.",
        )
        .await
        .expect("run_turn against live gateway");

    eprintln!("[plain_completion] model reply: {:?}", outcome.reply);

    assert!(
        !outcome.reply.trim().is_empty(),
        "expected a non-empty reply from the live model"
    );
    assert!(
        outcome.reply.to_ascii_uppercase().contains("PONG"),
        "expected the reply to contain PONG (case-insensitive), got: {:?}",
        outcome.reply
    );
}

/// 2. **streaming** — the `KnowledgeChatRuntime` does not surface a streaming
///    path, so per the task we exercise `LlmClient::chat_stream` directly
///    against the gateway. Assert ≥1 delta arrived and the concatenation is
///    non-empty.
#[tokio::test]
async fn streaming() {
    let Some(key) = gate("streaming") else {
        return;
    };

    let client = LlmClient::new(live_config(key));
    let prompt = Message::user("Count from 1 to 5, separated by spaces. Nothing else.");
    let messages: Vec<&Message> = vec![&prompt];

    let mut stream = client
        .chat_stream(&messages, &[])
        .await
        .expect("open stream against live gateway");

    let mut deltas = 0usize;
    let mut accumulated = String::new();
    let mut finished = false;

    while let Some(event) = stream.next().await {
        match event.expect("stream event") {
            StreamEvent::Delta { content } => {
                deltas += 1;
                accumulated.push_str(&content);
            }
            StreamEvent::Done { finish_reason } => {
                eprintln!("[streaming] done, finish_reason={finish_reason}");
                finished = true;
            }
            other => {
                eprintln!("[streaming] event: {other:?}");
            }
        }
    }

    eprintln!("[streaming] deltas={deltas} accumulated={accumulated:?} finished={finished}");

    assert!(
        deltas >= 1,
        "expected at least one streamed delta from the live model, got {deltas}"
    );
    assert!(
        !accumulated.trim().is_empty(),
        "expected non-empty accumulated stream content"
    );
}

/// 3. **tool_calling / knowledge-grounded** (the headline test) — seed the KB
///    with a distinctive fact (17-day return window), then ask the live model
///    to search the knowledge base. Assert:
///      (a) the model actually invoked the `knowledge_search` tool, and
///      (b) the final grounded answer contains "17".
///    This proves real tool-calling + RAG grounding through the live model.
#[tokio::test]
async fn tool_calling_knowledge_grounded() {
    let Some(key) = gate("tool_calling_knowledge_grounded") else {
        return;
    };

    let storage = seeded_storage();
    let runtime = KnowledgeChatRuntime::new(storage, live_config(key)).with_max_iterations(6);

    let outcome = runtime
        .run_turn(
            "e2e-tool",
            "What is SmooAI's return window? Search the knowledge base.",
        )
        .await
        .expect("run_turn against live gateway");

    eprintln!("[tool_calling] final reply: {:?}", outcome.reply);
    eprintln!(
        "[tool_calling] knowledge_search invoked: {}",
        outcome.invoked_tool("knowledge_search")
    );
    if let Some(result) = outcome.tool_result("knowledge_search") {
        eprintln!("[tool_calling] knowledge_search result: {result:?}");
    }

    // (a) The live model decided to call knowledge_search.
    assert!(
        outcome.invoked_tool("knowledge_search"),
        "expected the live model to invoke knowledge_search; events: {:?}",
        outcome.events
    );

    // (b) The final answer is grounded in the retrieved fact.
    assert!(
        outcome.reply.contains("17"),
        "expected the grounded answer to contain the retrieved 17-day fact, got: {:?}",
        outcome.reply
    );
}

/// 4. **multi_turn / context** (optional) — two turns on the same runtime +
///    conversation id, against the live model.
///
///    This test documents a real, verified limitation of the current reference
///    [`KnowledgeChatRuntime`]: **cross-turn memory is not yet wired.** Each
///    `run_turn` builds a *fresh* [`Agent`](smooth_operator::Agent) via
///    `build_agent`, and `Agent::new` assigns a new random agent id every time.
///    The engine's checkpoint-resume path (`Agent::resume_or_new`) keys on that
///    agent id, so turn 2's agent finds no checkpoint from turn 1 and starts
///    from a clean conversation. The runtime DOES persist both turns to the
///    storage adapter's message log, but it does not replay that log into the
///    agent's `prior_messages`.
///
///    Verified live (claude-haiku-4-5): turn 1 "My name is Zog." → turn 2
///    "What is my name?" returns "I don't have access to your personal
///    information…" — the model genuinely has no memory of turn 1, confirming
///    the gap is in the runtime wiring, not the gateway.
///
///    To make this assert a recall, `KnowledgeChatRuntime` would need to either
///    (a) reuse a single `Agent` (stable id) across turns, or (b) load the
///    persisted message log and pass it via `AgentConfig::with_prior_messages`,
///    keyed by `conversation_id`. Tracked as future runtime work — out of scope
///    for this live-gateway test, which only proves the engine ↔ gateway path.
///
///    The assertion below pins the CURRENT behavior so this test stays green
///    and flips loudly (forcing a doc update) once cross-turn memory lands.
#[tokio::test]
async fn multi_turn_context_documents_no_cross_turn_memory() {
    let Some(key) = gate("multi_turn_context_documents_no_cross_turn_memory") else {
        return;
    };

    let storage = Arc::new(InMemoryStorageAdapter::new());
    let runtime = KnowledgeChatRuntime::new(storage, live_config(key)).with_max_iterations(4);

    let first = runtime
        .run_turn("e2e-multi", "My name is Zog. Just acknowledge briefly.")
        .await
        .expect("first turn");
    eprintln!("[multi_turn] turn 1 reply: {:?}", first.reply);

    let second = runtime
        .run_turn("e2e-multi", "What is my name? Reply with just the name.")
        .await
        .expect("second turn");
    eprintln!("[multi_turn] turn 2 reply: {:?}", second.reply);

    // CURRENT behavior: no cross-turn memory, so the model cannot recall "Zog".
    // (If this assertion ever fails because the reply DOES contain "Zog", that's
    //  good news — cross-turn memory got wired — and this test should be flipped
    //  to assert recall.)
    assert!(
        !second.reply.to_ascii_uppercase().contains("ZOG"),
        "runtime now appears to carry cross-turn memory (turn 2 recalled 'Zog'): {:?} \
         — flip this test to assert recall and wire the docs accordingly",
        second.reply
    );
}
