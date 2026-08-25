//! Durable write-confirmations (th-db0816) — a pending confirmation must
//! survive the pod that parked it.
//!
//! The in-process park is a channel into a turn running on ONE pod. A visitor
//! whose refresh reconnects them to a different replica — or whose pod rolled —
//! used to get `NO_PENDING_CONFIRMATION` for an approval they were explicitly
//! asked to give. These tests drive the real `handler::handle_frame` on TWO
//! `AppState`s ("pods") sharing one storage adapter, mirroring the two-instance
//! pattern of the session-registry fix (th-ca579c):
//!
//!   - a park on instance A writes a durable `metadata.pendingConfirmation`
//!     record to shared storage;
//!   - `confirm_tool_action` on instance B — which holds NO live park — resolves
//!     it from that record: approved spawns a continuation turn that actually
//!     executes the tool (via the one-shot pre-approval, WITHOUT parking a
//!     second time), denied acks and retires the record;
//!   - the record is cleared either way, and the negative control proves
//!     `NO_PENDING_CONFIRMATION` is still reachable when no record exists (so
//!     the positive assertions mean something).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{Document, DocumentType};

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler::{self, SpawnedTurn, UserScope};
use smooth_operator_server::server::SEED_ORG_ID;
use smooth_operator_server::state::AppState;

/// A config that gates `knowledge_search` behind human confirmation (the same
/// always-registered tool the in-process HITL tests gate, so a real tool
/// exercises the full path without inventing a test-only write tool).
fn confirm_config() -> ServerConfig {
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

/// Seed one public doc so an approved `knowledge_search("alpha")` returns real
/// content the assertions can look for.
fn seeded_storage() -> Arc<InMemoryStorageAdapter> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let kb = storage.knowledge();
    let mut doc = Document::new(
        "The alpha office hours are open to the whole organization.",
        "handbook/hours.md",
        DocumentType::Documentation,
    );
    doc.id = "doc-public".to_string();
    kb.ingest(doc).expect("ingest public doc");
    storage
}

/// A mock that streams a gated `knowledge_search` call — the turn parks on it.
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
    ]);
    mock
}

/// A mock for the continuation "pod": re-issues the approved call (turn 1),
/// then answers (turn 2). The gated call must EXECUTE, not park — the one-shot
/// pre-approval is what these tests are proving.
fn continuation_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_2".into(),
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
            content: "Done — the approved search ran.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

fn scope() -> UserScope {
    UserScope::User("visitor@example.com".to_string())
}

/// Drive one frame and return the sink receiver plus any spawned turn, so the
/// caller can observe every event the frame produces (a parked turn keeps
/// emitting after the first one).
async fn drive(state: &AppState, frame: &Value) -> (UnboundedReceiver<Value>, Option<SpawnedTurn>) {
    let (tx, rx) = unbounded_channel::<Value>();
    let turn = handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        Some(SEED_ORG_ID),
        &scope(),
        &frame.to_string(),
        &tx,
    )
    .await;
    (rx, turn)
}

/// Receive events until one matches `pred` (bounded), returning it plus
/// everything seen along the way.
async fn recv_until(
    rx: &mut UnboundedReceiver<Value>,
    what: &str,
    pred: impl Fn(&Value) -> bool,
) -> (Value, Vec<Value>) {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                let hit = pred(&ev);
                seen.push(ev.clone());
                if hit {
                    return (ev, seen);
                }
            }
            Ok(None) => panic!("sink closed awaiting {what}; saw: {seen:?}"),
            Err(_) => panic!("timed out awaiting {what}; saw: {seen:?}"),
        }
    }
}

/// Create a session + conversation through the real create path and wait for
/// the spawned persistence to settle in shared storage.
async fn create_session(state: &AppState, storage: &InMemoryStorageAdapter) -> String {
    let frame = json!({
        "action": "create_conversation_session",
        "requestId": "cs",
        "agentId": uuid::Uuid::new_v4().to_string(),
    });
    let (mut rx, _) = drive(state, &frame).await;
    let (ev, _) = recv_until(&mut rx, "create response", |ev| {
        ev["type"] == "immediate_response"
    })
    .await;
    let session_id: String = ev["data"]["sessionId"].as_str().expect("sessionId").into();
    let conversation_id = ev["data"]["conversationId"]
        .as_str()
        .expect("conversationId")
        .to_string();
    for _ in 0..100 {
        let participants = storage
            .list_participants_by_conversation(&conversation_id)
            .await
            .expect("list participants");
        if participants.len() >= 2 && state.get_session(&session_id).is_some() {
            return session_id;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("create-session never persisted for {session_id}");
}

/// Park a confirmation-gated turn on `state` and return the sink receiver once
/// `write_confirmation_required` has been emitted AND the durable record has
/// landed in shared storage (the bridge persists it from a spawned task, so the
/// event can outrun the write).
async fn park_turn(
    state: &AppState,
    storage: &Arc<InMemoryStorageAdapter>,
    session_id: &str,
) -> UnboundedReceiver<Value> {
    let frame = json!({
        "action": "send_message",
        "requestId": "req-park",
        "sessionId": session_id,
        "message": "Search the handbook for alpha",
    });
    let (mut rx, turn) = drive(state, &frame).await;
    assert!(turn.is_some(), "send_message should spawn a turn");
    recv_until(&mut rx, "write_confirmation_required", |ev| {
        ev["type"] == "write_confirmation_required"
    })
    .await;
    for _ in 0..100 {
        if pending_record(storage, session_id).await.is_some() {
            return rx;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the park never persisted a pendingConfirmation record");
}

/// The durable `metadata.pendingConfirmation` record for a session, read
/// straight from shared storage (never through either instance's cache).
async fn pending_record(storage: &Arc<InMemoryStorageAdapter>, session_id: &str) -> Option<Value> {
    storage
        .get_session(session_id)
        .await
        .expect("get_session")
        .and_then(|s| s.metadata.as_ref()?.get("pendingConfirmation").cloned())
}

/// Every tool-result string the continuation's model read, concatenated.
fn tool_result_text(events: &[Value]) -> String {
    let mut s = String::new();
    for ev in events {
        if let Some(result) = ev
            .pointer("/data/state/rawResponse/toolResult/result")
            .and_then(Value::as_str)
        {
            s.push_str(result);
            s.push('\n');
        }
    }
    s
}

#[tokio::test]
async fn an_approval_landing_on_another_instance_still_executes_the_tool() {
    let storage = seeded_storage();
    let pod_a = AppState::new(storage.clone(), confirm_config())
        .with_chat_provider(Arc::new(parking_mock()));
    let pod_b = AppState::new(storage.clone(), confirm_config())
        .with_chat_provider(Arc::new(continuation_mock()));

    let session_id = create_session(&pod_a, &storage).await;

    // Park on pod A. The durable record must carry what any pod needs to carry
    // out the verdict: the tool, its arguments, and the human-readable prompt.
    let _rx_a = park_turn(&pod_a, &storage, &session_id).await;
    let record = pending_record(&storage, &session_id)
        .await
        .expect("record present after park");
    assert_eq!(record["tool"], "knowledge_search");
    assert_eq!(record["arguments"]["query"], "alpha");
    assert!(record["prompt"].as_str().is_some_and(|p| !p.is_empty()));

    // Premise check (mirrors the th-ca579c tests): pod B holds NO live park —
    // without this, the test would pass even if the two pods shared memory.
    assert!(
        pod_b.take_confirmation(&session_id).is_none(),
        "pod B must not hold the park locally, or this test proves nothing"
    );

    // Approve on pod B. The continuation turn must stream a full reply on THIS
    // socket, the gated tool must actually execute (its real KB result reaches
    // the model), and it must do so WITHOUT parking a second time.
    let confirm = json!({
        "action": "confirm_tool_action",
        "requestId": "req-confirm",
        "sessionId": session_id,
        "approved": true,
    });
    let (mut rx_b, turn_b) = drive(&pod_b, &confirm).await;
    assert!(
        turn_b.is_some(),
        "resolving a durable record must spawn a continuation turn"
    );
    let (_, seen) = recv_until(&mut rx_b, "eventual_response", |ev| {
        ev["type"] == "eventual_response"
    })
    .await;
    // A real toolResult proves the call EXECUTED. A still-gated call would
    // never produce one on this socket (it parks), and a hook-denied call
    // produces a "blocked by hook" rejection result instead. (The handler path
    // scopes knowledge per-org, so the seeded doc's text is not asserted —
    // execution vs. park/block is what this test is about.)
    let result = tool_result_text(&seen);
    assert!(
        !result.is_empty() && !result.contains("blocked"),
        "the approved tool must actually run; events: {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|ev| ev["type"] == "write_confirmation_required"),
        "the continuation must not park a second time; events: {seen:?}"
    );

    // The record is retired: a duplicate confirm cannot execute the write again.
    let mut cleared = false;
    for _ in 0..100 {
        if pending_record(&storage, &session_id).await.is_none() {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        cleared,
        "the durable record must be cleared after resolution"
    );
}

#[tokio::test]
async fn a_decline_landing_on_another_instance_retires_the_record_without_a_turn() {
    let storage = seeded_storage();
    let pod_a = AppState::new(storage.clone(), confirm_config())
        .with_chat_provider(Arc::new(parking_mock()));
    let pod_b = AppState::new(storage.clone(), confirm_config())
        .with_chat_provider(Arc::new(continuation_mock()));

    let session_id = create_session(&pod_a, &storage).await;
    let _rx_a = park_turn(&pod_a, &storage, &session_id).await;

    let confirm = json!({
        "action": "confirm_tool_action",
        "requestId": "req-deny",
        "sessionId": session_id,
        "approved": false,
    });
    let (mut rx_b, turn_b) = drive(&pod_b, &confirm).await;
    assert!(turn_b.is_none(), "a decline must not spawn a turn");
    let (ev, _) = recv_until(&mut rx_b, "ack", |ev| ev["type"] == "immediate_response").await;
    assert_eq!(ev["status"], 200, "got: {ev}");
    assert_eq!(ev["data"]["approved"], false, "got: {ev}");

    let mut cleared = false;
    for _ in 0..100 {
        if pending_record(&storage, &session_id).await.is_none() {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cleared, "a declined record must be retired too");
}

/// NEGATIVE CONTROL: with no live park AND no durable record, the confirm still
/// gets `NO_PENDING_CONFIRMATION` — so the positive tests above are proving the
/// record path, not a fallback that approves anything.
#[tokio::test]
async fn a_confirm_with_no_record_anywhere_is_still_refused() {
    let storage = seeded_storage();
    let pod_a = AppState::new(storage.clone(), confirm_config())
        .with_chat_provider(Arc::new(parking_mock()));
    let pod_b = AppState::new(storage.clone(), confirm_config())
        .with_chat_provider(Arc::new(continuation_mock()));

    // A session that never parked anything.
    let session_id = create_session(&pod_a, &storage).await;

    let confirm = json!({
        "action": "confirm_tool_action",
        "requestId": "req-none",
        "sessionId": session_id,
        "approved": true,
    });
    let (mut rx_b, turn_b) = drive(&pod_b, &confirm).await;
    assert!(turn_b.is_none());
    let (ev, _) = recv_until(&mut rx_b, "refusal", |ev| ev["type"] == "error").await;
    assert_eq!(ev["error"]["code"], "NO_PENDING_CONFIRMATION", "got: {ev}");
}
