//! `cancelled` has to be the LAST thing a client sees for that requestId.
//!
//! `turn_cancel.rs` covers the case `JoinHandle::abort()` handles well: a turn parked
//! on a real `.await` is dropped there and produces nothing more. This file covers the
//! case it does NOT handle — a turn that is executing rather than suspended.
//!
//! `abort()` is asynchronous. It marks the task and the runtime drops the future the
//! next time the task *yields*; a task in the middle of a synchronous stretch keeps
//! running, and an `.await` on an already-`Ready` future (which every in-memory storage
//! and mock call is) does not yield either. So a turn that reaches its tail in that
//! window runs the whole thing — dispatching an OTP, persisting, emitting
//! `eventual_response` — after the client was told the turn was cancelled. One
//! requestId, a 499 and a 200.
//!
//! The window is driven deterministically instead of slept on: the tool blocks the
//! worker thread with a plain `std::sync::mpsc` receive. A task that never yields
//! cannot be aborted, so the cancel is guaranteed to have landed (flag raised, abort
//! issued, `cancelled` on the wire) before the turn resumes into its tail.
//!
//! Needs a multi-thread runtime: the tool parks a worker thread outright, and the
//! connection's reader has to stay free to receive the `cancel` frame.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

const BLOCKING_TOOL: &str = "blocking_probe";

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

/// A tool that BLOCKS the worker thread rather than awaiting. Deliberate: a task that
/// never yields to the runtime cannot be aborted, so releasing it resumes a turn whose
/// abort is already pending — exactly the state a cancel-racing-completion produces.
struct BlockingTool {
    started: Arc<AtomicBool>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

#[async_trait]
impl Tool for BlockingTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: BLOCKING_TOOL.into(),
            description: "holds the turn's thread for cancellation tests".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _arguments: Value) -> anyhow::Result<String> {
        self.started.store(true, Ordering::SeqCst);
        let rx = {
            let mut slot = self.release.lock().expect("release lock");
            slot.take()
        };
        if let Some(rx) = rx {
            // Bounded so a broken test fails rather than hangs the suite.
            let _ = rx.recv_timeout(Duration::from_secs(20));
        }
        Ok("done".into())
    }
}

struct BlockingToolProvider {
    started: Arc<AtomicBool>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

#[async_trait]
impl ToolProvider for BlockingToolProvider {
    async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        let rx = {
            let mut slot = self.release.lock().expect("release lock");
            slot.take()
        };
        vec![Arc::new(BlockingTool {
            started: self.started.clone(),
            release: Mutex::new(rx),
        })]
    }
}

/// Turn 1 calls the blocking tool; turn 1's second model call answers, which is what
/// drives the turn all the way into its tail once the tool is released.
fn blocking_tool_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: BLOCKING_TOOL.into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: "{}".into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ]);
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: "I should never have been sent.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

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

async fn recv_within(client: &mut common::Client, dur: Duration) -> Option<Value> {
    match tokio::time::timeout(dur, client.next()).await {
        Ok(Some(Ok(WsMessage::Text(t)))) => Some(serde_json::from_str(&t).expect("parse json")),
        Ok(Some(Ok(WsMessage::Close(_)))) | Ok(None) => None,
        Ok(Some(Ok(_))) => None,
        Ok(Some(Err(e))) => panic!("ws error: {e}"),
        Err(_) => None,
    }
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_emitted_after_cancelled() {
    let started = Arc::new(AtomicBool::new(false));
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let provider = Arc::new(BlockingToolProvider {
        started: started.clone(),
        release: Mutex::new(Some(release_rx)),
    });

    let state = build_state(keyless_config())
        .with_chat_provider(Arc::new(blocking_tool_mock()))
        .with_tools(provider);
    let url = common::boot_state(state).await;
    let mut client = common::connect(&url).await;
    let session_id = create_session(&mut client).await;

    common::send_json(
        &mut client,
        &json!({
            "action": "send_message",
            "requestId": "turn-1",
            "sessionId": session_id,
            "message": "please do the blocking thing",
        }),
    )
    .await;

    // The turn is provably inside the tool, holding its worker thread.
    wait_until("turn parked in the blocking tool", || {
        started.load(Ordering::SeqCst)
    })
    .await;

    common::send_json(
        &mut client,
        &json!({ "action": "cancel", "requestId": "turn-1" }),
    )
    .await;

    let mut seen: Vec<Value> = Vec::new();
    let cancelled =
        common::recv_until(&mut client, "cancelled", &mut seen, Duration::from_secs(5)).await;
    assert_eq!(cancelled["requestId"], "turn-1", "got: {cancelled}");
    assert_eq!(cancelled["status"], 499, "got: {cancelled}");

    // Only now let the turn run on. Its abort is already pending, but it is executing
    // rather than suspended, so it proceeds into the tail on its own.
    let _ = release_tx.send(());

    if let Some(ev) = recv_within(&mut client, Duration::from_secs(3)).await {
        panic!(
            "cancelled must be terminal, but a {} arrived after it: {ev}",
            ev["type"]
        );
    }
}
