//! Host injection-seam tests (runner level, offline, `MockLlmClient`).
//!
//! Two seams let a host run the operator's chat turn with its OWN tool catalog
//! and its OWN per-org persona WITHOUT forking the runner:
//!
//! * **SEAM 1 — custom tools.** A [`ToolProvider`] contributes EXTRA tools that
//!   the runner merges into the turn's `ToolRegistry` alongside the built-ins.
//! * **SEAM 2 — per-org persona.** A resolved `system_prompt` overrides the
//!   built-in const prompt for the turn.
//!
//! Both must be behavior-preserving by default: no provider ⇒ built-ins only;
//! no persona ⇒ the existing const prompt. We assert that by inspecting the
//! [`MockLlmClient`]'s recorded calls — the tool schemas offered to the model
//! (SEAM 1) and the system message the model received (SEAM 2).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{LlmConfig, Role, Tool, ToolSchema};

use smooth_operator_server::runner::{self, TurnRequest, TurnResult};

/// The built-in system prompt the runner falls back to with no persona. Kept in
/// sync with `runner.rs`'s `KNOWLEDGE_CHAT_SYSTEM_PROMPT` (its opening clause is
/// stable enough to assert on without coupling to the full text).
const CONST_PROMPT_OPENING: &str = "You are a helpful customer-support agent";

/// A throwaway LLM config (never actually called — the mock answers).
fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// A trivial host tool, used to prove an injected tool's schema reaches the LLM.
struct StubHostTool;

#[async_trait]
impl Tool for StubHostTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "host_crm_lookup".into(),
            description: "Look up a customer in the host CRM.".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _arguments: Value) -> anyhow::Result<String> {
        Ok("ok".into())
    }
}

/// The per-turn facts a provider observed, captured for assertions.
#[derive(Default)]
struct SeenCtx {
    org_id: Option<String>,
    conversation_id: Option<String>,
    gateway_key: Option<String>,
}

/// A provider that contributes [`StubHostTool`] and records the per-turn context
/// it saw (org + conversation + gateway key).
struct StubProvider {
    seen: Arc<std::sync::Mutex<SeenCtx>>,
}

#[async_trait]
impl ToolProvider for StubProvider {
    async fn tools_for(&self, ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        let mut seen = self.seen.lock().unwrap();
        seen.org_id = ctx.org_id.clone();
        seen.conversation_id = ctx.conversation_id.clone();
        seen.gateway_key = ctx.gateway_key.clone();
        vec![Arc::new(StubHostTool) as Arc<dyn Tool>]
    }
}

/// Drive one real `run_streaming_turn` with the given seam inputs and return the
/// result plus the mock so the test can assert on what the model received. The
/// model is scripted to answer immediately (no tool call) — the assertions are
/// about the REQUEST the runner built (offered tools + system prompt), not the
/// model's behavior.
async fn run_turn(
    tool_provider: Option<Arc<dyn ToolProvider>>,
    system_prompt: Option<String>,
    org_id: Option<String>,
) -> (TurnResult, MockLlmClient) {
    run_turn_with_key(tool_provider, system_prompt, org_id, None).await
}

/// Like [`run_turn`] but also threads a resolved per-turn gateway key, so a test
/// can assert the provider sees it.
async fn run_turn_with_key(
    tool_provider: Option<Arc<dyn ToolProvider>>,
    system_prompt: Option<String>,
    org_id: Option<String>,
    gateway_key: Option<String>,
) -> (TurnResult, MockLlmClient) {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());

    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: "Done.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    let (tx, rx): (_, UnboundedReceiver<Value>) = unbounded_channel();
    let result = runner::run_streaming_turn(
        TurnRequest {
            storage,
            llm: mock_llm(),
            max_iterations: 4,
            conversation_id: "conv-seam",
            request_id: "req-1",
            user_message: "hello",
            access: AccessContext::anonymous(),
            llm_provider: Some(Arc::new(mock.clone())),
            reranker: None,
            confirmation: None,
            tool_provider,
            system_prompt,
            org_id,
            gateway_key,
            workflow: None,
            judge: None,
            greeting_section: None,
            enabled_tools: None,
        },
        &tx,
    )
    .await
    .expect("run_streaming_turn");

    drop(tx);
    // Drain the protocol events so the channel closes cleanly.
    let mut rx = rx;
    while rx.recv().await.is_some() {}

    (result, mock)
}

/// The system-prompt text the model saw on the first call (the `System` message).
fn system_prompt_seen(mock: &MockLlmClient) -> String {
    let calls = mock.calls();
    let first = calls.first().expect("at least one LLM call");
    first
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .map(|m| m.content.clone())
        .expect("a system message was sent")
}

/// The tool names offered to the model on the first call.
fn tool_names_seen(mock: &MockLlmClient) -> Vec<String> {
    let calls = mock.calls();
    let first = calls.first().expect("at least one LLM call");
    first.tools.iter().map(|t| t.name.clone()).collect()
}

// ---------------------------------------------------------------------------
// SEAM 1 — custom tool injection
// ---------------------------------------------------------------------------

/// Default: no tool provider ⇒ the registry is exactly the built-ins. The only
/// tool the model is offered is `knowledge_search`; no host tool appears.
#[tokio::test]
async fn no_tool_provider_offers_only_builtins() {
    let (_r, mock) = run_turn(None, None, None).await;
    let names = tool_names_seen(&mock);
    assert_eq!(
        names,
        vec!["knowledge_search".to_string()],
        "with no provider the registry must be exactly today's built-ins"
    );
}

/// Injected provider ⇒ its tool is merged into the registry alongside the
/// built-in, so the model is offered BOTH. The provider also sees the turn's
/// org_id so a host can return per-org tools.
#[tokio::test]
async fn injected_provider_tools_reach_the_model() {
    let seen = Arc::new(std::sync::Mutex::new(SeenCtx::default()));
    let provider = Arc::new(StubProvider { seen: seen.clone() });

    let (_r, mock) = run_turn(Some(provider), None, Some("org-acme".into())).await;

    let mut names = tool_names_seen(&mock);
    names.sort();
    assert_eq!(
        names,
        vec![
            "host_crm_lookup".to_string(),
            "knowledge_search".to_string()
        ],
        "the injected host tool must be merged with the built-ins"
    );
    assert_eq!(
        seen.lock().unwrap().org_id.as_deref(),
        Some("org-acme"),
        "the provider must receive the turn's org_id for per-org tools"
    );
}

/// The provider must see the turn's `conversation_id` and resolved `gateway_key`
/// so a host's conversation-persisting tools and retrieval tools work (instead of
/// degrading to no-ops on an empty conversation id / missing key).
#[tokio::test]
async fn provider_sees_conversation_id_and_gateway_key() {
    let seen = Arc::new(std::sync::Mutex::new(SeenCtx::default()));
    let provider = Arc::new(StubProvider { seen: seen.clone() });

    let (_r, _mock) = run_turn_with_key(
        Some(provider),
        None,
        Some("org-acme".into()),
        Some("sk-org-acme".into()),
    )
    .await;

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.conversation_id.as_deref(),
        Some("conv-seam"),
        "the provider must receive the turn's conversation_id"
    );
    assert_eq!(
        seen.gateway_key.as_deref(),
        Some("sk-org-acme"),
        "the provider must receive the resolved per-org gateway key"
    );
}

// ---------------------------------------------------------------------------
// SEAM 2 — per-org agent persona
// ---------------------------------------------------------------------------

/// Default: no persona ⇒ the model receives the built-in const prompt.
#[tokio::test]
async fn no_persona_uses_const_prompt() {
    let (_r, mock) = run_turn(None, None, None).await;
    let prompt = system_prompt_seen(&mock);
    assert!(
        prompt.starts_with(CONST_PROMPT_OPENING),
        "with no persona the runner must use the const prompt, got: {prompt}"
    );
}

/// A resolved persona ⇒ it REPLACES the const prompt as the turn's system prompt.
#[tokio::test]
async fn persona_overrides_const_prompt() {
    let persona = "You are Acme's brisk, no-nonsense concierge. Be terse.";
    let (_r, mock) = run_turn(None, Some(persona.to_string()), None).await;
    let prompt = system_prompt_seen(&mock);
    assert_eq!(
        prompt, persona,
        "a resolved persona must be used verbatim as the system prompt"
    );
    assert!(
        !prompt.starts_with(CONST_PROMPT_OPENING),
        "the const prompt must NOT leak through when a persona is set"
    );
}
