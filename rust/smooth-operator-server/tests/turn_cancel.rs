//! User-initiated turn cancellation — the `cancel` action ("Stop button").
//!
//! This is the REFERENCE behaviour the polyglot servers (TS/Python/Go/.NET) and
//! the frontend mirror. It proves, over a real WebSocket:
//!
//!   1. **Cancel mid-turn stops it.** A `cancel` frame while a turn is parked in a
//!      tool aborts the turn *future* — the in-flight `.await` is abandoned (a
//!      drop-guard flag flips, the tool never reaches its post-await line) — and a
//!      terminal `cancelled` event is emitted. No `eventual_response` follows.
//!   2. **Cancel with no active turn is a silent no-op** (no event; connection
//!      stays live).
//!   3. **A normal turn still completes** with an `eventual_response` (cancellation
//!      wiring doesn't disturb the happy path).
//!   4. **Disconnect mid-turn also aborts the turn** (no client remains to receive
//!      its output).
//!
//! Runs fully offline: a `MockLlmClient` scripts the turn and a host `ToolProvider`
//! installs a deterministic tool that parks the turn on a long sleep, giving a
//! stable in-flight window to cancel in. No network / gateway key.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use futures_util::StreamExt;

use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{Tool, ToolSchema};

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::server::build_state;

const SLOW_TOOL: &str = "slow_probe";

/// A keyless config — the mock chat provider serves the turn, so no gateway key
/// is needed (the handler uses a placeholder LLM config when a chat provider is
/// injected).
fn keyless_config() -> ServerConfig {
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
        confirm_tools: Vec::new(),
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

/// Flips its flag to `true` when dropped. Held across the tool's `.await`, so a
/// cancelled (dropped) turn future flips it — the positive signal that the future
/// was abandoned mid-await rather than run to completion.
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A tool that parks the turn: it records that it started, then sleeps far longer
/// than any test. If the turn is cancelled the sleep's `.await` is dropped, so
/// `finished` never flips and `dropped` (via the guard) does.
struct SlowTool {
    started: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for SlowTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: SLOW_TOOL.into(),
            description: "parks the turn for cancellation tests".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _arguments: Value) -> anyhow::Result<String> {
        self.started.store(true, Ordering::SeqCst);
        let _guard = DropFlag(self.dropped.clone());
        tokio::time::sleep(Duration::from_secs(3600)).await;
        // Only reached if the turn was NOT cancelled.
        self.finished.store(true, Ordering::SeqCst);
        Ok("done".into())
    }
}

struct SlowToolProvider {
    started: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

#[async_trait]
impl ToolProvider for SlowToolProvider {
    async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(SlowTool {
            started: self.started.clone(),
            finished: self.finished.clone(),
            dropped: self.dropped.clone(),
        })]
    }
}

/// A mock that streams a single call to the slow tool (so the turn parks in the
/// tool and never returns on its own).
fn slow_tool_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: SLOW_TOOL.into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: "{}".into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ]);
    mock
}

/// A mock that just answers (no tool) so a turn completes normally.
fn answer_mock(text: &str) -> MockLlmClient {
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

/// Poll `cond` until it holds or a short deadline elapses; fail otherwise.
async fn wait_until(label: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {label}");
}

/// Try to receive one JSON event within `dur`; `None` on timeout (no event).
async fn recv_within(client: &mut common::Client, dur: Duration) -> Option<Value> {
    match tokio::time::timeout(dur, client.next()).await {
        Ok(Some(Ok(WsMessage::Text(t)))) => Some(serde_json::from_str(&t).expect("parse json")),
        Ok(Some(Ok(WsMessage::Close(_)))) | Ok(None) => None,
        Ok(Some(Ok(_))) => None,
        Ok(Some(Err(e))) => panic!("ws error: {e}"),
        Err(_) => None, // timed out — no event arrived
    }
}

/// Create a session and return its id.
async fn create_session(client: &mut common::Client) -> String {
    common::send_json(
        client,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-1",
            "agentId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    let created = common::recv_json(client).await;
    assert_eq!(created["type"], "immediate_response", "got: {created}");
    created["data"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

#[tokio::test]
async fn cancel_mid_turn_aborts_and_emits_cancelled() {
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(SlowToolProvider {
        started: started.clone(),
        finished: finished.clone(),
        dropped: dropped.clone(),
    });

    let state = build_state(keyless_config())
        .with_chat_provider(Arc::new(slow_tool_mock()))
        .with_tools(provider);
    let url = common::boot_state(state).await;
    let mut client = common::connect(&url).await;

    let session_id = create_session(&mut client).await;

    // Start a turn; it will park inside the slow tool.
    common::send_json(
        &mut client,
        &json!({
            "action": "send_message",
            "requestId": "turn-1",
            "sessionId": session_id,
            "message": "please do the slow thing",
        }),
    )
    .await;

    // Wait until the turn is genuinely in flight (parked in the tool's await).
    wait_until("turn parked in tool", || started.load(Ordering::SeqCst)).await;
    assert!(
        !finished.load(Ordering::SeqCst),
        "tool must not have finished yet"
    );

    // Cancel it (reusing the turn's requestId, the correlation convention).
    common::send_json(
        &mut client,
        &json!({ "action": "cancel", "requestId": "turn-1" }),
    )
    .await;

    // A terminal `cancelled` event arrives, echoing the turn's requestId. (Skip
    // any ack/stream events that were in flight before the cancel landed.)
    let mut seen = Vec::new();
    let cancelled =
        common::recv_until(&mut client, "cancelled", &mut seen, Duration::from_secs(5)).await;
    assert_eq!(cancelled["requestId"], "turn-1", "got: {cancelled}");
    assert_eq!(cancelled["status"], 499, "got: {cancelled}");
    assert_eq!(cancelled["data"]["requestId"], "turn-1");

    // The turn future was dropped mid-await: the drop-guard fired and the tool's
    // post-await line never ran.
    wait_until("turn future dropped", || dropped.load(Ordering::SeqCst)).await;
    assert!(
        !finished.load(Ordering::SeqCst),
        "cancelled turn's tool must never reach its post-await completion"
    );

    // No further terminal event (no eventual_response) follows the cancellation.
    let after = recv_within(&mut client, Duration::from_millis(500)).await;
    assert!(
        after.is_none(),
        "no event should follow the cancellation, got: {after:?}"
    );

    // Connection is still alive and usable.
    common::send_json(&mut client, &json!({ "action": "ping", "requestId": "p1" })).await;
    let pong = common::recv_json(&mut client).await;
    assert_eq!(pong["type"], "pong");
    assert_eq!(pong["requestId"], "p1");
}

#[tokio::test]
async fn cancel_with_no_active_turn_is_a_noop() {
    let state = build_state(keyless_config()).with_chat_provider(Arc::new(answer_mock("hi")));
    let url = common::boot_state(state).await;
    let mut client = common::connect(&url).await;
    let _session_id = create_session(&mut client).await;

    // Cancel with nothing running: must emit nothing.
    common::send_json(
        &mut client,
        &json!({ "action": "cancel", "requestId": "nope" }),
    )
    .await;

    // The next event is the pong (the cancel produced no event of its own).
    common::send_json(&mut client, &json!({ "action": "ping", "requestId": "p1" })).await;
    let ev = common::recv_json(&mut client).await;
    assert_eq!(
        ev["type"], "pong",
        "cancel must not emit an event; got: {ev}"
    );
    assert_eq!(ev["requestId"], "p1");
}

#[tokio::test]
async fn normal_turn_still_completes() {
    let state =
        build_state(keyless_config()).with_chat_provider(Arc::new(answer_mock("All done here.")));
    let url = common::boot_state(state).await;
    let mut client = common::connect(&url).await;
    let session_id = create_session(&mut client).await;

    common::send_json(
        &mut client,
        &json!({
            "action": "send_message",
            "requestId": "turn-ok",
            "sessionId": session_id,
            "message": "hello",
        }),
    )
    .await;

    let mut seen = Vec::new();
    let done = common::recv_until(
        &mut client,
        "eventual_response",
        &mut seen,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(done["requestId"], "turn-ok", "got: {done}");
    assert_eq!(done["status"], 200);
    // No cancellation happened.
    assert!(
        !seen.iter().any(|e| e["type"] == "cancelled"),
        "a normal turn must not emit a cancelled event"
    );
}

#[tokio::test]
async fn disconnect_mid_turn_aborts_the_turn() {
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(SlowToolProvider {
        started: started.clone(),
        finished: finished.clone(),
        dropped: dropped.clone(),
    });

    let state = build_state(keyless_config())
        .with_chat_provider(Arc::new(slow_tool_mock()))
        .with_tools(provider);
    let url = common::boot_state(state).await;
    let mut client = common::connect(&url).await;
    let session_id = create_session(&mut client).await;

    common::send_json(
        &mut client,
        &json!({
            "action": "send_message",
            "requestId": "turn-x",
            "sessionId": session_id,
            "message": "please do the slow thing",
        }),
    )
    .await;
    wait_until("turn parked in tool", || started.load(Ordering::SeqCst)).await;

    // Client hangs up mid-turn.
    drop(client);

    // The server aborts the in-flight turn: the future is dropped (guard fires),
    // and the tool never reaches its post-await completion.
    wait_until("turn future dropped on disconnect", || {
        dropped.load(Ordering::SeqCst)
    })
    .await;
    assert!(
        !finished.load(Ordering::SeqCst),
        "disconnect must abort the turn before it completes"
    );
}
