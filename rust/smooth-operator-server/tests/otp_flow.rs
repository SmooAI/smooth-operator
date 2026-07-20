//! End-user OTP identity-verification seam — the `verify_otp` action path.
//!
//! Drives the real `handler::handle_frame` so the credential-accepting surface
//! is exercised exactly as a client hits it. A stub [`OtpService`] stands in for
//! the host (the reference server never generates or validates a code itself):
//!   - a `Verified` outcome → `otp_verified` **and** the session is now marked
//!     identity-verified on `AppState`;
//!   - an `Invalid` outcome → `otp_invalid` carrying the host's remaining-attempt
//!     count + machine-readable reason;
//!   - **no OtpService installed** → fail closed with `otp_invalid` (`NOT_FOUND`);
//!   - an unknown session id → `error` (`SESSION_NOT_FOUND`) — a code can't
//!     authenticate a session the server doesn't track (adversarial input);
//!   - a missing `code` → validation error.
//!
//! Replay of a consumed/expired code is the host's contract (it owns the code
//! store + attempt accounting): the server reflects whatever outcome the host
//! returns, so a replayed code simply surfaces as the host's `Invalid`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::otp::{OtpContact, OtpDelivery, OtpError, OtpService, OtpVerifyOutcome};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler;
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

/// A stub host OTP service returning a fixed outcome for `verify_otp`. `send_otp`
/// always "delivers" to a masked email (unused by the verify tests, present so
/// the trait is fully implemented).
struct StubOtp {
    outcome: OtpVerifyOutcome,
}

#[async_trait]
impl OtpService for StubOtp {
    async fn send_otp(
        &self,
        _session_id: &str,
        _contact: &OtpContact,
    ) -> anyhow::Result<OtpDelivery> {
        Ok(OtpDelivery {
            channel: smooth_operator::otp::OtpChannel::Email,
            masked_destination: "j***@example.com".into(),
        })
    }
    async fn verify_otp(&self, _session_id: &str, _code: &str) -> OtpVerifyOutcome {
        self.outcome.clone()
    }
}

/// Create a session (with an email contact) and return its id.
async fn create_session(state: &AppState) -> String {
    let (tx, mut rx) = unbounded_channel::<Value>();
    let frame = json!({
        "action": "create_conversation_session",
        "requestId": "cs-1",
        "agentId": "agent-otp",
        "userName": "Alice",
        "userEmail": "alice@example.com",
    });
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        None,
        &smooth_operator_server::handler::UserScope::Unscoped,
        &frame.to_string(),
        &tx,
    )
    .await;
    let ev = recv(&mut rx).await;
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    ev["data"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

/// Send a `verify_otp` frame for `session_id` + `code` and return the event.
async fn verify(state: &AppState, session_id: &str, code: &str) -> Value {
    let (tx, mut rx) = unbounded_channel::<Value>();
    let frame = json!({
        "action": "verify_otp",
        "requestId": "vo-1",
        "sessionId": session_id,
        "code": code,
    });
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        None,
        &smooth_operator_server::handler::UserScope::Unscoped,
        &frame.to_string(),
        &tx,
    )
    .await;
    recv(&mut rx).await
}

async fn recv(rx: &mut UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an event should be emitted")
        .expect("sink open")
}

#[tokio::test]
async fn verify_otp_success_marks_session_authenticated() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage, base_config()).with_otp_service(Arc::new(StubOtp {
        outcome: OtpVerifyOutcome::Verified,
    }));
    let session_id = create_session(&state).await;

    assert!(
        !state.session_authenticated(&session_id),
        "session starts unverified"
    );

    let ev = verify(&state, &session_id, "123456").await;
    assert_eq!(ev["type"], "otp_verified", "got: {ev}");
    assert_eq!(ev["requestId"], "vo-1");
    assert_eq!(
        ev["data"]["data"]["message"],
        "Identity verified successfully."
    );
    assert!(
        state.session_authenticated(&session_id),
        "a verified code must mark the session authenticated"
    );
}

#[tokio::test]
async fn verify_otp_invalid_reflects_host_attempts_and_reason() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage, base_config()).with_otp_service(Arc::new(StubOtp {
        outcome: OtpVerifyOutcome::Invalid {
            attempts_remaining: 2,
            error: Some(OtpError::InvalidCode),
            message: "Invalid code. 2 attempt(s) remaining.".into(),
        },
    }));
    let session_id = create_session(&state).await;

    let ev = verify(&state, &session_id, "000000").await;
    assert_eq!(ev["type"], "otp_invalid", "got: {ev}");
    assert_eq!(ev["data"]["data"]["attemptsRemaining"], 2);
    assert_eq!(ev["data"]["data"]["error"], "INVALID_CODE");
    assert!(
        !state.session_authenticated(&session_id),
        "a rejected code must NOT authenticate the session"
    );
}

#[tokio::test]
async fn verify_otp_without_service_fails_closed() {
    // No OtpService installed → verification is impossible; fail closed on the
    // documented otp_invalid path, session stays unverified.
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage, base_config());
    let session_id = create_session(&state).await;

    let ev = verify(&state, &session_id, "123456").await;
    assert_eq!(ev["type"], "otp_invalid", "got: {ev}");
    assert_eq!(ev["data"]["data"]["error"], "NOT_FOUND");
    assert_eq!(ev["data"]["data"]["attemptsRemaining"], 0);
    assert!(!state.session_authenticated(&session_id));
}

#[tokio::test]
async fn verify_otp_unknown_session_errors() {
    // Adversarial: a code for a session the server doesn't track must not
    // authenticate anything — it's a clean SESSION_NOT_FOUND error.
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage, base_config()).with_otp_service(Arc::new(StubOtp {
        outcome: OtpVerifyOutcome::Verified,
    }));

    let ev = verify(&state, "no-such-session", "123456").await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(ev["error"]["code"], "SESSION_NOT_FOUND");
}

#[tokio::test]
async fn verify_otp_missing_code_is_validation_error() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage, base_config()).with_otp_service(Arc::new(StubOtp {
        outcome: OtpVerifyOutcome::Verified,
    }));
    let session_id = create_session(&state).await;

    let (tx, mut rx) = unbounded_channel::<Value>();
    let frame = json!({
        "action": "verify_otp",
        "requestId": "vo-1",
        "sessionId": session_id,
    });
    handler::handle_frame(
        &state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        None,
        &smooth_operator_server::handler::UserScope::Unscoped,
        &frame.to_string(),
        &tx,
    )
    .await;
    let ev = recv(&mut rx).await;
    assert_eq!(ev["type"], "error", "got: {ev}");
    assert_eq!(ev["error"]["code"], "VALIDATION_ERROR");
}
