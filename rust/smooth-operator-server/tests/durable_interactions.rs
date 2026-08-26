//! Durable Rich-Interaction parks (th-db0816) — the interaction sibling of
//! `durable_confirmation.rs`.
//!
//! A raise's park is a channel into a turn on ONE pod. A visitor whose refresh
//! reconnected them to another replica — or whose pod rolled — used to get
//! `NO_PENDING_INTERACTION` for the card they were just shown, and the identity
//! they typed evaporated. These tests drive the real `handler::handle_frame` on
//! TWO `AppState`s sharing one storage adapter:
//!
//!   - a raise on instance A persists a durable `metadata.pendingInteraction`
//!     record (id + kind + spec — the full validation contract);
//!   - `submit_interaction` on instance B — no live park — validates against
//!     that record and resolves it: submitted values run the host effect (the
//!     identity attach, now written through to storage), declined retires it;
//!   - a mismatched `interactionId` is still rejected, and the negative control
//!     proves `NO_PENDING_INTERACTION` is still reachable with no record.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler::{self, SpawnedTurn, UserScope};
use smooth_operator_server::server::SEED_ORG_ID;
use smooth_operator_server::state::AppState;

fn base_config() -> ServerConfig {
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

/// A mock that raises `request_identity_intake` (email required, name
/// optional) — the turn parks inside the raise tool.
fn raising_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_raise".into(),
            name: "request_identity_intake".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk:
                r#"{"fields":[{"key":"email","required":true},{"key":"name","required":false}],"reason":"to send you the quote"}"#
                    .into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ]);
    mock
}

fn scope() -> UserScope {
    UserScope::User("visitor@example.com".to_string())
}

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

/// Create a session that declares the `identity_form` capability (so the raise
/// takes the RICH path and parks), and wait for persistence to settle.
async fn create_rich_session(state: &AppState, storage: &InMemoryStorageAdapter) -> String {
    let frame = json!({
        "action": "create_conversation_session",
        "requestId": "cs",
        "agentId": uuid::Uuid::new_v4().to_string(),
        "supports": ["identity_form"],
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

/// Park an identity_intake raise on `state`; return the raised interactionId
/// once the event has surfaced AND the durable record has landed in storage.
async fn park_raise(
    state: &AppState,
    storage: &Arc<InMemoryStorageAdapter>,
    session_id: &str,
) -> String {
    let frame = json!({
        "action": "send_message",
        "requestId": "req-raise",
        "sessionId": session_id,
        "message": "Can you send me a quote?",
    });
    let (mut rx, turn) = drive(state, &frame).await;
    assert!(turn.is_some(), "send_message should spawn a turn");
    let (ev, _) = recv_until(&mut rx, "interaction_required", |ev| {
        ev["type"] == "interaction_required"
    })
    .await;
    let interaction_id = ev["data"]["data"]["interactionId"]
        .as_str()
        .expect("interactionId")
        .to_string();
    for _ in 0..100 {
        if pending_record(storage, session_id).await.is_some() {
            return interaction_id;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the raise never persisted a pendingInteraction record");
}

async fn pending_record(storage: &Arc<InMemoryStorageAdapter>, session_id: &str) -> Option<Value> {
    storage
        .get_session(session_id)
        .await
        .expect("get_session")
        .and_then(|s| s.metadata.as_ref()?.get("pendingInteraction").cloned())
}

/// The session's stored metadata value for `key`, straight from storage.
async fn stored_meta(
    storage: &Arc<InMemoryStorageAdapter>,
    session_id: &str,
    key: &str,
) -> Option<Value> {
    storage
        .get_session(session_id)
        .await
        .expect("get_session")
        .and_then(|s| s.metadata.as_ref()?.get(key).cloned())
}

#[tokio::test]
async fn a_submit_landing_on_another_instance_still_attaches_the_identity() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let pod_a =
        AppState::new(storage.clone(), base_config()).with_chat_provider(Arc::new(raising_mock()));
    let pod_b = AppState::new(storage.clone(), base_config())
        .with_chat_provider(Arc::new(MockLlmClient::new()));

    let session_id = create_rich_session(&pod_a, &storage).await;
    let interaction_id = park_raise(&pod_a, &storage, &session_id).await;

    // The record carries the full validation contract.
    let record = pending_record(&storage, &session_id)
        .await
        .expect("record after raise");
    assert_eq!(record["kind"], "identity_intake");
    assert_eq!(record["spec"]["fields"][0]["key"], "email");

    // Premise: pod B holds no live park.
    assert!(
        pod_b.pending_interaction(&session_id).is_none(),
        "pod B must not hold the park locally, or this test proves nothing"
    );

    // A mismatched interactionId is still rejected — durable validation is
    // real validation.
    let stale = json!({
        "action": "submit_interaction",
        "requestId": "req-stale",
        "sessionId": session_id,
        "interactionId": "stale-id",
        "values": { "email": "kim@example.com" },
    });
    let (mut rx_stale, _) = drive(&pod_b, &stale).await;
    let (ev, _) = recv_until(&mut rx_stale, "mismatch", |ev| ev["type"] == "error").await;
    assert_eq!(ev["error"]["code"], "INTERACTION_MISMATCH", "got: {ev}");
    assert!(
        pending_record(&storage, &session_id).await.is_some(),
        "a rejected submit must leave the record for a retry"
    );

    // The real submit on pod B: resolves, attaches the identity DURABLY, and
    // retires the record.
    let submit = json!({
        "action": "submit_interaction",
        "requestId": "req-submit",
        "sessionId": session_id,
        "interactionId": interaction_id,
        "values": { "email": "kim@example.com", "name": "Kim" },
    });
    let (mut rx_b, _) = drive(&pod_b, &submit).await;
    let (ev, _) = recv_until(&mut rx_b, "submit ack", |ev| {
        ev["type"] == "immediate_response"
    })
    .await;
    assert_eq!(ev["status"], 200, "got: {ev}");
    assert_eq!(ev["data"]["kind"], "identity_intake", "got: {ev}");

    // The attach wrote THROUGH to storage — the whole point: the identity the
    // visitor typed survives every pod.
    let mut attached = false;
    for _ in 0..100 {
        if stored_meta(&storage, &session_id, "contactEmail").await
            == Some(Value::from("kim@example.com"))
        {
            attached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        attached,
        "the submitted identity must be durable in storage"
    );
    assert_eq!(
        stored_meta(&storage, &session_id, "userName").await,
        Some(Value::from("Kim"))
    );

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
        "the durable record must be retired after resolution"
    );
}

#[tokio::test]
async fn a_decline_landing_on_another_instance_retires_the_record() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let pod_a =
        AppState::new(storage.clone(), base_config()).with_chat_provider(Arc::new(raising_mock()));
    let pod_b = AppState::new(storage.clone(), base_config())
        .with_chat_provider(Arc::new(MockLlmClient::new()));

    let session_id = create_rich_session(&pod_a, &storage).await;
    let interaction_id = park_raise(&pod_a, &storage, &session_id).await;

    let decline = json!({
        "action": "submit_interaction",
        "requestId": "req-decline",
        "sessionId": session_id,
        "interactionId": interaction_id,
        "declined": true,
    });
    let (mut rx_b, _) = drive(&pod_b, &decline).await;
    let (ev, _) = recv_until(&mut rx_b, "decline ack", |ev| {
        ev["type"] == "immediate_response"
    })
    .await;
    assert_eq!(ev["status"], 200, "got: {ev}");
    assert_eq!(ev["data"]["declined"], true, "got: {ev}");

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

/// NEGATIVE CONTROL: with no live park AND no durable record, the submit still
/// gets `NO_PENDING_INTERACTION`.
#[tokio::test]
async fn a_submit_with_no_record_anywhere_is_still_refused() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let pod_a =
        AppState::new(storage.clone(), base_config()).with_chat_provider(Arc::new(raising_mock()));
    let pod_b = AppState::new(storage.clone(), base_config())
        .with_chat_provider(Arc::new(MockLlmClient::new()));

    let session_id = create_rich_session(&pod_a, &storage).await;

    let submit = json!({
        "action": "submit_interaction",
        "requestId": "req-none",
        "sessionId": session_id,
        "interactionId": "whatever",
        "values": { "email": "kim@example.com" },
    });
    let (mut rx_b, _) = drive(&pod_b, &submit).await;
    let (ev, _) = recv_until(&mut rx_b, "refusal", |ev| ev["type"] == "error").await;
    assert_eq!(ev["error"]["code"], "NO_PENDING_INTERACTION", "got: {ev}");
}
