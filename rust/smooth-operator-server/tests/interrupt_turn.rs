//! `interrupt` — the protocol's Stop button (th-3a912a).
//!
//! Before this, a turn was spawned detached with no handle: nothing could stop
//! it, so a user watching the agent go off the rails could only send *another*
//! message — spawning a second concurrent turn — and wait. These tests prove a
//! running turn is registered, cancellable by conversation, closes itself out
//! on its own `requestId`, and deregisters afterwards.
//!
//! The "stuck turn" is produced honestly rather than with a sleep: the mock LLM
//! calls a confirmation-gated tool, so the turn parks inside the HITL hook
//! awaiting a `confirm_tool_action` that never arrives. That is a turn which
//! genuinely hangs forever without an interrupt — if the cancellation did not
//! work, these tests would time out instead of passing.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use smooth_operator::access_control::AccessContext;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler;
use smooth_operator_server::state::AppState;

/// Config gating `knowledge_search` behind human confirmation, so a turn that
/// calls it parks indefinitely — our stand-in for "the agent is being weird".
fn parking_config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "claude-haiku-4-5".into(),
        seed_kb: false,
        max_iterations: 4,
        max_tokens: 128,
        storage: StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: vec!["knowledge_search".into()],
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

/// A mock whose first stream calls the gated tool (parking the turn) and whose
/// second would answer — the second is never reached while the turn is parked.
fn parking_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "knowledge_search".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: r#"{"query":"alpha"}"#.into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Delta {
            content: "Here is what I found.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

fn parking_state() -> AppState {
    AppState::new(Arc::new(InMemoryStorageAdapter::new()), parking_config())
        .with_chat_provider(Arc::new(parking_mock()))
}

/// A plain state with no mock — used by the validation-path tests, which never
/// reach an LLM.
fn bare_state() -> AppState {
    let mut config = parking_config();
    config.confirm_tools = Vec::new();
    AppState::new(Arc::new(InMemoryStorageAdapter::new()), config)
}

/// Dispatch one frame, emitting into the SHARED `tx` so a test observes the
/// running turn's events and its own ack on the same sink the real connection
/// would use.
async fn drive_into(state: &AppState, frame: &Value, tx: &UnboundedSender<Value>) {
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        None,
        &frame.to_string(),
        tx,
    )
    .await;
}

/// Dispatch a frame on a private sink and return its first event.
async fn drive(state: &AppState, frame: &Value) -> Value {
    let (tx, mut rx) = unbounded_channel::<Value>();
    drive_into(state, frame, &tx).await;
    recv(&mut rx).await
}

async fn recv(rx: &mut UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an event should be emitted")
        .expect("sink open")
}

/// Receive until an event of `event_type` arrives, collecting what came before.
/// Bounded, so a failure to cancel surfaces as a clear timeout panic rather
/// than a hung test.
async fn recv_until(rx: &mut UnboundedReceiver<Value>, event_type: &str) -> (Value, Vec<Value>) {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                let hit = ev["type"] == event_type;
                seen.push(ev.clone());
                if hit {
                    return (ev, seen);
                }
            }
            Ok(None) => panic!("sink closed before '{event_type}'; saw: {seen:?}"),
            Err(_) => panic!("timed out waiting for '{event_type}'; saw: {seen:?}"),
        }
    }
}

/// Create a session and return `(sessionId, conversationId)`.
async fn create_session(state: &AppState) -> (String, String) {
    let ev = drive(
        state,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-1",
            "agentId": "agent-1",
        }),
    )
    .await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    (
        ev["data"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string(),
        ev["data"]["conversationId"]
            .as_str()
            .expect("conversationId")
            .to_string(),
    )
}

/// Send a message and wait until the turn has genuinely parked on the gated
/// tool (`write_confirmation_required`), i.e. it is now unstoppable-by-waiting.
async fn park_a_turn(
    state: &AppState,
    session_id: &str,
    request_id: &str,
    tx: &UnboundedSender<Value>,
    rx: &mut UnboundedReceiver<Value>,
) {
    drive_into(
        state,
        &json!({
            "action": "send_message",
            "requestId": request_id,
            "sessionId": session_id,
            "message": "Tell me about alpha",
        }),
        tx,
    )
    .await;
    let _ = recv_until(rx, "write_confirmation_required").await;
}

#[tokio::test]
async fn interrupt_stops_a_parked_turn_and_closes_it_out() {
    let state = parking_state();
    let (session_id, conversation_id) = create_session(&state).await;
    let (tx, mut rx) = unbounded_channel::<Value>();

    park_a_turn(&state, &session_id, "req-turn-1", &tx, &mut rx).await;
    assert!(
        state.has_active_turn(&conversation_id),
        "a running turn must be registered as interruptible"
    );

    drive_into(
        &state,
        &json!({ "action": "interrupt", "requestId": "int-1", "sessionId": session_id }),
        &tx,
    )
    .await;

    // The interrupted TURN closes out on its OWN requestId — that is what the
    // client's streaming state is keyed by, so this is what unsticks the UI.
    let (closing, _seen) = recv_until(&mut rx, "eventual_response").await;
    assert_eq!(closing["requestId"], "req-turn-1", "got: {closing}");
    assert_eq!(closing["status"], 200, "got: {closing}");
    let parts = closing["data"]["data"]["response"]["responseParts"]
        .as_array()
        .expect("responseParts");
    assert_eq!(parts.len(), 1, "got: {closing}");
    assert!(
        parts[0].as_str().unwrap_or_default().contains("Stopped"),
        "the reply must record that the user stopped the turn: {closing}"
    );

    // Deregistered, so the button reports honestly next time.
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.has_active_turn(&conversation_id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the cancelled turn must deregister itself");

    // The park is torn down too: a late confirm verdict has nowhere to land, so
    // it cannot leak into whatever runs next on this session.
    let ev = drive(
        &state,
        &json!({
            "action": "confirm_tool_action",
            "requestId": "cf-late",
            "sessionId": session_id,
            "approved": true,
        }),
    )
    .await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(ev["error"]["code"], "NO_PENDING_CONFIRMATION", "got: {ev}");
}

#[tokio::test]
async fn interrupt_acks_the_interrupting_client() {
    let state = parking_state();
    let (session_id, conversation_id) = create_session(&state).await;
    let (tx, mut rx) = unbounded_channel::<Value>();

    park_a_turn(&state, &session_id, "req-turn-1", &tx, &mut rx).await;
    drive_into(
        &state,
        &json!({ "action": "interrupt", "requestId": "int-1", "sessionId": session_id }),
        &tx,
    )
    .await;

    let (ack, _seen) = recv_until(&mut rx, "immediate_response").await;
    assert_eq!(
        ack["requestId"], "int-1",
        "the ack echoes the interrupt frame"
    );
    assert_eq!(ack["status"], 200, "got: {ack}");
    assert_eq!(ack["data"]["conversationId"], conversation_id, "got: {ack}");
}

#[tokio::test]
async fn interrupt_accepts_a_conversation_id_directly() {
    // A reconnected client holds the conversation, not the session that started
    // the turn — turns are tracked per conversation exactly so this works.
    let state = parking_state();
    let (session_id, conversation_id) = create_session(&state).await;
    let (tx, mut rx) = unbounded_channel::<Value>();

    park_a_turn(&state, &session_id, "req-turn-1", &tx, &mut rx).await;
    drive_into(
        &state,
        &json!({
            "action": "interrupt",
            "requestId": "int-1",
            "conversationId": conversation_id,
        }),
        &tx,
    )
    .await;

    let (closing, _seen) = recv_until(&mut rx, "eventual_response").await;
    assert_eq!(closing["requestId"], "req-turn-1", "got: {closing}");
}

#[tokio::test]
async fn a_second_turn_runs_after_an_interrupt() {
    // Stopping a weird turn must not wedge the conversation.
    let state = parking_state();
    let (session_id, conversation_id) = create_session(&state).await;
    let (tx, mut rx) = unbounded_channel::<Value>();

    park_a_turn(&state, &session_id, "req-turn-1", &tx, &mut rx).await;
    drive_into(
        &state,
        &json!({ "action": "interrupt", "requestId": "int-1", "sessionId": session_id }),
        &tx,
    )
    .await;
    let _ = recv_until(&mut rx, "eventual_response").await;

    // The mock's remaining script answers directly (no gated tool), so this turn
    // completes on its own.
    drive_into(
        &state,
        &json!({
            "action": "send_message",
            "requestId": "req-turn-2",
            "sessionId": session_id,
            "message": "Try again",
        }),
        &tx,
    )
    .await;
    assert!(
        state.has_active_turn(&conversation_id),
        "the follow-up turn is registered and itself interruptible"
    );

    let (done, _seen) = recv_until(&mut rx, "eventual_response").await;
    assert_eq!(done["requestId"], "req-turn-2", "got: {done}");
    let parts = done["data"]["data"]["response"]["responseParts"]
        .as_array()
        .expect("responseParts");
    assert!(
        parts[0]
            .as_str()
            .unwrap_or_default()
            .contains("Here is what I found"),
        "the follow-up turn produces a real answer: {done}"
    );
}

#[tokio::test]
async fn interrupt_with_no_running_turn_reports_no_active_turn() {
    let state = bare_state();
    let (session_id, conversation_id) = create_session(&state).await;

    let ev = drive(
        &state,
        &json!({ "action": "interrupt", "requestId": "int-1", "sessionId": session_id }),
    )
    .await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(ev["error"]["code"], "NO_ACTIVE_TURN", "got: {ev}");
    assert!(
        !state.has_active_turn(&conversation_id),
        "nothing was registered"
    );
}

#[tokio::test]
async fn interrupt_for_an_unknown_session_is_rejected() {
    let state = bare_state();
    let ev = drive(
        &state,
        &json!({ "action": "interrupt", "requestId": "int-1", "sessionId": "sess-nope" }),
    )
    .await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(ev["error"]["code"], "SESSION_NOT_FOUND", "got: {ev}");
}

#[tokio::test]
async fn interrupt_without_a_target_is_rejected() {
    let state = bare_state();
    let ev = drive(
        &state,
        &json!({ "action": "interrupt", "requestId": "int-1" }),
    )
    .await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(ev["error"]["code"], "VALIDATION_ERROR", "got: {ev}");
}

#[tokio::test]
async fn interrupt_for_an_unknown_conversation_reports_no_active_turn() {
    // An explicit conversationId is not resolved through the session map, so a
    // bogus one is simply "nothing running" rather than a session error.
    let state = bare_state();
    let ev = drive(
        &state,
        &json!({
            "action": "interrupt",
            "requestId": "int-1",
            "conversationId": "conv-nope",
        }),
    )
    .await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(ev["error"]["code"], "NO_ACTIVE_TURN", "got: {ev}");
}

#[tokio::test]
async fn a_double_interrupt_reports_the_turn_is_already_gone() {
    let state = parking_state();
    let (session_id, conversation_id) = create_session(&state).await;
    let (tx, mut rx) = unbounded_channel::<Value>();

    park_a_turn(&state, &session_id, "req-turn-1", &tx, &mut rx).await;
    drive_into(
        &state,
        &json!({ "action": "interrupt", "requestId": "int-1", "sessionId": session_id }),
        &tx,
    )
    .await;
    let _ = recv_until(&mut rx, "eventual_response").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.has_active_turn(&conversation_id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("deregistered");

    let ev = drive(
        &state,
        &json!({ "action": "interrupt", "requestId": "int-2", "sessionId": session_id }),
    )
    .await;
    assert_eq!(ev["error"]["code"], "NO_ACTIVE_TURN", "got: {ev}");
}
