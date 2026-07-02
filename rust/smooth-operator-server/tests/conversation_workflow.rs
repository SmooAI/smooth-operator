//! Conversation-workflow + per-agent-instructions parity (SMOODEV-590).
//!
//! Drives [`runner::run_streaming_turn`] offline (mock LLMs, no gateway key) to
//! prove the per-turn side of the workflow feature:
//!   - the agent's `instructions` drive the system prompt (persona honored),
//!   - a configured workflow injects the current step's intent/criteria into the
//!     system prompt,
//!   - the judge advances the step on `yes` and holds it on `no` / failure,
//!   - the turn never fails on the judge.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc::unbounded_channel;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::agent_config::{ConversationWorkflow, ConversationWorkflowStep};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::conversation::Role;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::LlmConfig;

use smooth_operator_server::runner::{self, TurnRequest, WorkflowTurn};

const CONVERSATION_ID: &str = "conv-wf-1";
const REQUEST_ID: &str = "req-wf-1";

fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// A main-turn mock that streams a single plain-text reply (no tool calls).
fn reply_mock(text: &str) -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: text.into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

fn workflow() -> ConversationWorkflow {
    ConversationWorkflow {
        goal: "Assess transformation posture".into(),
        steps: vec![
            ConversationWorkflowStep {
                id: "greet".into(),
                intent: "Greet and confirm the caller's name".into(),
                criteria: "The user's name has been captured".into(),
                next: None,
            },
            ConversationWorkflowStep {
                id: "collect".into(),
                intent: "Ask what tooling they use today".into(),
                criteria: "At least one current tool named".into(),
                next: None,
            },
        ],
    }
}

/// Run one turn with an optional workflow + judge, returning the result plus the
/// system prompt the MAIN model saw (first System message of its first call).
async fn run_turn(
    system_prompt: Option<String>,
    workflow: Option<WorkflowTurn>,
    judge: Option<Arc<MockLlmClient>>,
    main_reply: &str,
) -> (runner::TurnResult, String) {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let main = reply_mock(main_reply);
    let main_arc = Arc::new(main);
    let (tx, _rx) = unbounded_channel::<Value>();

    let result = runner::run_streaming_turn(
        TurnRequest {
            storage,
            llm: mock_llm(),
            max_iterations: 4,
            conversation_id: CONVERSATION_ID,
            request_id: REQUEST_ID,
            user_message: "Hi, I'm Dana",
            access: AccessContext::anonymous(),
            llm_provider: Some(main_arc.clone()),
            reranker: None,
            confirmation: None,
            tool_provider: None,
            system_prompt,
            org_id: None,
            gateway_key: None,
            workflow,
            judge: judge.map(|j| j as Arc<dyn smooth_operator_core::llm_provider::LlmProvider>),
        },
        &tx,
    )
    .await
    .expect("run_streaming_turn");

    // The first recorded call's first System message is the resolved prompt.
    let calls = main_arc.calls();
    let system = calls
        .first()
        .and_then(|c| c.messages.iter().find(|m| m.role == Role::System))
        .map(|m| m.content.clone())
        .unwrap_or_default();
    (result, system)
}

#[tokio::test]
async fn instructions_drive_system_prompt_not_the_default_persona() {
    // `system_prompt` is what the handler resolves from the agent's instructions.
    let persona =
        "You are the Transformation Posture assistant. You are NOT a generic support agent.";
    let (_result, system) = run_turn(Some(persona.to_string()), None, None, "Hello Dana!").await;
    assert!(
        system.contains("Transformation Posture assistant"),
        "system prompt should carry the agent's instructions, got: {system}"
    );
    assert!(
        !system.contains("customer-support agent"),
        "the default customer-support persona must not leak in: {system}"
    );
}

#[tokio::test]
async fn workflow_section_injected_for_current_step() {
    let judge = Arc::new(MockLlmClient::new());
    judge.push_text("no"); // don't advance; we only assert the injected section
    let (result, system) = run_turn(
        Some("You are the Posture assistant.".into()),
        Some(WorkflowTurn {
            workflow: workflow(),
            current_step_id: None, // fresh → first step
        }),
        Some(judge),
        "Nice to meet you!",
    )
    .await;

    assert!(
        system.contains("<ConversationWorkflow>"),
        "workflow block missing: {system}"
    );
    assert!(
        system.contains("CURRENT STEP (1/2): greet"),
        "wrong step rendered: {system}"
    );
    assert!(
        system.contains("Assess transformation posture"),
        "goal missing: {system}"
    );
    // Judge said "no" → stay on the first step.
    assert_eq!(result.next_step_id.as_deref(), Some("greet"));
}

#[tokio::test]
async fn judge_advances_step_on_yes() {
    let judge = Arc::new(MockLlmClient::new());
    judge.push_text("yes");
    let (result, _system) = run_turn(
        None,
        Some(WorkflowTurn {
            workflow: workflow(),
            current_step_id: Some("greet".into()),
        }),
        Some(judge.clone()),
        "Great to meet you, Dana!",
    )
    .await;
    assert_eq!(result.next_step_id.as_deref(), Some("collect"));
    // The judge was actually consulted (one chat call).
    assert_eq!(judge.call_count(), 1);
}

#[tokio::test]
async fn judge_holds_step_on_no() {
    let judge = Arc::new(MockLlmClient::new());
    judge.push_text("no");
    let (result, _system) = run_turn(
        None,
        Some(WorkflowTurn {
            workflow: workflow(),
            current_step_id: Some("greet".into()),
        }),
        Some(judge),
        "How can I help?",
    )
    .await;
    assert_eq!(result.next_step_id.as_deref(), Some("greet"));
}

#[tokio::test]
async fn judge_failure_stays_on_current_step() {
    // A judge that errors must NOT fail the turn or advance — stay put.
    let judge = Arc::new(MockLlmClient::new());
    judge.push_error("gateway exploded");
    let (result, _system) = run_turn(
        None,
        Some(WorkflowTurn {
            workflow: workflow(),
            current_step_id: Some("greet".into()),
        }),
        Some(judge),
        "Hi there",
    )
    .await;
    assert_eq!(result.next_step_id.as_deref(), Some("greet"));
}

#[tokio::test]
async fn no_workflow_means_no_step_tracking() {
    let (result, system) = run_turn(None, None, None, "Hello").await;
    assert!(result.next_step_id.is_none());
    // Freeform: no workflow block, falls back to the const customer-support prompt.
    assert!(!system.contains("<ConversationWorkflow>"));
    assert!(system.contains("customer-support agent"));
}

#[tokio::test]
async fn terminal_step_yes_stays_on_terminal() {
    let judge = Arc::new(MockLlmClient::new());
    judge.push_text("yes");
    let (result, _system) = run_turn(
        None,
        Some(WorkflowTurn {
            workflow: workflow(),
            current_step_id: Some("collect".into()), // last step
        }),
        Some(judge),
        "We use Salesforce and Slack.",
    )
    .await;
    // Terminal step: yes verdict keeps us on the last step (workflow complete).
    assert_eq!(result.next_step_id.as_deref(), Some("collect"));
}
