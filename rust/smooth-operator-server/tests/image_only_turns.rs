//! Image-only sends and image history replay (SMOODEV-3706).
//!
//! Two bugs made a photo sent from a phone useless:
//!   - `send_message` rejected an empty `message` before it looked at `images`,
//!     so a photo with no caption failed with `VALIDATION_ERROR`;
//!   - history replay rebuilt every prior message as text only, so on the NEXT
//!     turn the model could no longer see a photo it had just been shown.
//!
//! Over a real WebSocket with a scripted `MockLlmClient`, whose recorded calls
//! show exactly what the model was sent.

mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use smooth_operator_core::conversation::Role;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::server::build_state;

const PHOTO: &str = "data:image/png;base64,iVBORw0KGgo=";

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

fn push_answer(mock: &MockLlmClient, text: &str) {
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: text.into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
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

async fn run_turn(client: &mut common::Client, frame: serde_json::Value) {
    let request_id = frame["requestId"].clone();
    common::send_json(client, &frame).await;
    let mut seen = Vec::new();
    let done = common::recv_until(
        client,
        "eventual_response",
        &mut seen,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(done["requestId"], request_id, "got: {done}");
    assert_eq!(done["status"], 200);
}

#[tokio::test]
async fn an_image_only_send_runs_and_the_next_turn_still_sees_the_photo() {
    let mock = MockLlmClient::new();
    push_answer(&mock, "That is a cat.");
    push_answer(&mock, "Orange.");
    let state = build_state(keyless_config()).with_chat_provider(Arc::new(mock.clone()));
    let url = common::boot_state(state).await;
    let mut client = common::connect(&url).await;
    let session_id = create_session(&mut client).await;

    // A photo with no caption.
    run_turn(
        &mut client,
        json!({
            "action": "send_message",
            "requestId": "turn-photo",
            "sessionId": session_id,
            "message": "",
            "images": [{ "url": PHOTO }],
        }),
    )
    .await;
    let first = mock.calls().first().cloned().expect("the model was called");
    let user = first
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .expect("a user message");
    assert_eq!(user.images.len(), 1, "the photo reached the model");
    assert_eq!(user.images[0].url, PHOTO);

    // A follow-up: the model must still be shown the photo from history.
    run_turn(
        &mut client,
        json!({
            "action": "send_message",
            "requestId": "turn-followup",
            "sessionId": session_id,
            "message": "what colour is it?",
        }),
    )
    .await;
    let second = mock.calls().last().cloned().expect("a second call");
    let replayed: Vec<_> = second
        .messages
        .iter()
        .filter(|m| m.role == Role::User && !m.images.is_empty())
        .collect();
    assert_eq!(
        replayed.len(),
        1,
        "the earlier photo is replayed: {:?}",
        second.messages
    );
    assert_eq!(replayed[0].images[0].url, PHOTO);
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.content == "That is a cat."),
        "the reply to the photo is replayed too"
    );
}

#[tokio::test]
async fn a_blank_send_with_no_attachment_is_still_rejected() {
    let state = build_state(keyless_config()).with_chat_provider(Arc::new(MockLlmClient::new()));
    let url = common::boot_state(state).await;
    let mut client = common::connect(&url).await;
    let session_id = create_session(&mut client).await;

    for frame in [
        json!({ "action": "send_message", "requestId": "r1", "sessionId": session_id, "message": "   " }),
        json!({ "action": "send_message", "requestId": "r2", "sessionId": session_id }),
        // A malformed images array is dropped fail-soft, so it is no attachment.
        json!({ "action": "send_message", "requestId": "r3", "sessionId": session_id, "message": "", "images": "nope" }),
    ] {
        common::send_json(&mut client, &frame).await;
        let err = common::recv_json(&mut client).await;
        assert_eq!(err["type"], "error", "got: {err}");
        assert_eq!(err["error"]["code"], "VALIDATION_ERROR", "got: {err}");
        assert_eq!(err["requestId"], frame["requestId"]);
    }
}
