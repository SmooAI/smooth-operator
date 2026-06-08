//! Per-invocation action dispatch for the API Gateway WebSocket Lambda.
//!
//! Unlike the reference axum server — one long-lived socket, one in-process
//! session registry, one outbound `mpsc` sink — API Gateway WebSocket invokes
//! this Lambda **once per inbound frame** with no socket and no in-process state
//! carried across invocations. So this module:
//!
//! 1. Keeps **no** session map in memory: sessions are created/read straight
//!    from the DynamoDB [`StorageAdapter`] (`create_session` / `get_session`),
//!    which is the durable source of truth across invocations.
//! 2. Posts events **back** through the API Gateway Management API
//!    ([`ConnectionPoster`]) instead of writing to a socket sink.
//! 3. Reuses the reference server's wire-protocol builders
//!    ([`smooth_operator_server::protocol`]) and its streaming, memory-
//!    carrying turn runner ([`smooth_operator_server::runner`]) verbatim —
//!    only the transport differs, so the protocol bytes and the turn semantics
//!    are identical to the server's.
//!
//! ### Streaming bridge
//! `runner::run_streaming_turn` emits events through an
//! `UnboundedSender<Value>` sink. We give it the sender half of a channel and,
//! concurrently, drain the receiver half — `post`ing each event to the
//! connection as it arrives. That preserves real-time token streaming over a
//! transport that has no socket: the runner fills the channel while the drain
//! task forwards to the Management API.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use smooth_operator::adapter::StorageAdapter;
use smooth_operator::domain::{
    Conversation, Participant, ParticipantType, Platform, Session, SessionStatus,
};
use smooth_operator_server::{protocol, runner};

use crate::config::LambdaConfig;
use crate::poster::ConnectionPoster;

/// The agent's display name.
const AGENT_NAME: &str = "smooth-agent";

/// Handle one inbound `$default` / route-key frame: parse the action envelope,
/// run it against DynamoDB, and post every produced event back to the
/// connection via `poster`.
///
/// Returns `Ok(())` regardless of protocol-level failures — those surface as
/// `error` events to the client, never as a hard Lambda error (which would
/// drop the connection / retry).
pub async fn handle_frame(
    storage: &Arc<dyn StorageAdapter>,
    config: &LambdaConfig,
    poster: &ConnectionPoster,
    raw: &str,
) -> Result<()> {
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            poster
                .post(&protocol::error(
                    None,
                    "VALIDATION_ERROR",
                    &format!("invalid JSON frame: {e}"),
                ))
                .await?;
            return Ok(());
        }
    };

    let action = parsed.get("action").and_then(Value::as_str);
    let request_id = parsed.get("requestId").and_then(Value::as_str);

    match action {
        Some("ping") => {
            poster.post(&protocol::pong(request_id)).await?;
        }
        Some("create_conversation_session") => {
            create_session(storage, config, poster, &parsed, request_id).await?;
        }
        Some("get_session") => {
            get_session(storage, poster, &parsed, request_id).await?;
        }
        Some("send_message") => {
            send_message(storage, config, poster, &parsed, request_id).await?;
        }
        Some(other) => {
            poster
                .post(&protocol::error(
                    request_id,
                    "UNSUPPORTED_ACTION",
                    &format!("action '{other}' is not supported"),
                ))
                .await?;
        }
        None => {
            poster
                .post(&protocol::error(
                    request_id,
                    "VALIDATION_ERROR",
                    "missing 'action' field",
                ))
                .await?;
        }
    }
    Ok(())
}

/// `create_conversation_session` — create a conversation + user/agent
/// participants + a session in DynamoDB, then reply with an
/// `immediate_response` carrying the session descriptor.
async fn create_session(
    storage: &Arc<dyn StorageAdapter>,
    config: &LambdaConfig,
    poster: &ConnectionPoster,
    parsed: &Value,
    request_id: Option<&str>,
) -> Result<()> {
    let agent_id = parsed
        .get("agentId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let user_name = parsed
        .get("userName")
        .and_then(Value::as_str)
        .unwrap_or("Visitor")
        .to_string();
    let user_email = parsed
        .get("userEmail")
        .and_then(Value::as_str)
        .map(str::to_string);
    let browser_fingerprint = parsed
        .get("browserFingerprint")
        .and_then(Value::as_str)
        .map(str::to_string);

    let now = chrono::Utc::now();
    let org_id = config.org_id.clone();

    let conversation_id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();
    let user_participant_id = uuid::Uuid::new_v4().to_string();
    let agent_participant_id = uuid::Uuid::new_v4().to_string();

    let conversation = Conversation {
        id: conversation_id.clone(),
        platform: Platform::Web,
        name: format!("Session {session_id}"),
        organization_id: org_id.clone(),
        idempotency_key: session_id.clone(),
        metadata_json: parsed.get("metadata").cloned(),
        analytics_json: None,
        created_at: now,
        updated_at: now,
    };

    let user_participant = Participant {
        id: user_participant_id.clone(),
        conversation_id: conversation_id.clone(),
        organization_id: org_id.clone(),
        participant_type: ParticipantType::User,
        external_id: None,
        internal_id: None,
        browser_fingerprint,
        browser_info: None,
        name: user_name,
        email: user_email,
        phone: None,
        crm_contact_id: None,
        metadata_json: None,
        created_at: now,
        updated_at: now,
    };

    let agent_participant = Participant {
        id: agent_participant_id.clone(),
        conversation_id: conversation_id.clone(),
        organization_id: org_id.clone(),
        participant_type: ParticipantType::AiAgent,
        external_id: None,
        internal_id: Some(agent_id.clone()),
        browser_fingerprint: None,
        browser_info: None,
        name: AGENT_NAME.to_string(),
        email: None,
        phone: None,
        crm_contact_id: None,
        metadata_json: None,
        created_at: now,
        updated_at: now,
    };

    let session = Session {
        session_id: session_id.clone(),
        conversation_id: conversation_id.clone(),
        agent_id: agent_id.clone(),
        agent_name: AGENT_NAME.to_string(),
        user_participant_id: user_participant_id.clone(),
        agent_participant_id: agent_participant_id.clone(),
        // The thread id is the conversation id: per-session memory is carried by
        // replaying this conversation's persisted message log (see runner).
        thread_id: conversation_id.clone(),
        status: Some(SessionStatus::Active),
        token_count: Some(0),
        message_count: Some(0),
        metadata: None,
        created_at: Some(now),
        updated_at: Some(now),
        ended_at: None,
        last_activity_at: Some(now),
    };

    // Persist to DynamoDB; any failure surfaces as a clean error event.
    if let Err(e) = storage.create_conversation(conversation).await {
        return poster
            .post(&protocol::error(
                request_id,
                "INTERNAL_ERROR",
                &format!("create conversation failed: {e}"),
            ))
            .await
            .map(|_| ());
    }
    if let Err(e) = storage.add_participant(user_participant).await {
        return poster
            .post(&protocol::error(
                request_id,
                "INTERNAL_ERROR",
                &format!("add user participant failed: {e}"),
            ))
            .await
            .map(|_| ());
    }
    if let Err(e) = storage.add_participant(agent_participant).await {
        return poster
            .post(&protocol::error(
                request_id,
                "INTERNAL_ERROR",
                &format!("add agent participant failed: {e}"),
            ))
            .await
            .map(|_| ());
    }
    if let Err(e) = storage.create_session(session).await {
        return poster
            .post(&protocol::error(
                request_id,
                "INTERNAL_ERROR",
                &format!("create session failed: {e}"),
            ))
            .await
            .map(|_| ());
    }

    let data = json!({
        "sessionId": session_id,
        "conversationId": conversation_id,
        "agentId": agent_id,
        "agentName": AGENT_NAME,
        "userParticipantId": user_participant_id,
        "agentParticipantId": agent_participant_id,
    });
    poster
        .post(&protocol::immediate_response(
            request_id,
            200,
            "Session created",
            data,
        ))
        .await?;
    Ok(())
}

/// `get_session` — read the session snapshot straight from DynamoDB.
async fn get_session(
    storage: &Arc<dyn StorageAdapter>,
    poster: &ConnectionPoster,
    parsed: &Value,
    request_id: Option<&str>,
) -> Result<()> {
    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        poster
            .post(&protocol::error(
                request_id,
                "VALIDATION_ERROR",
                "missing 'sessionId'",
            ))
            .await?;
        return Ok(());
    };

    match storage.get_session(session_id).await {
        Ok(Some(s)) => {
            let data = json!({
                "sessionId": s.session_id,
                "conversationId": s.conversation_id,
                "agentId": s.agent_id,
                "agentName": s.agent_name,
                "userParticipantId": s.user_participant_id,
                "agentParticipantId": s.agent_participant_id,
                "threadId": s.thread_id,
                "status": s.status.map_or("active", |st| match st {
                    SessionStatus::Active => "active",
                    SessionStatus::Idle => "idle",
                    SessionStatus::Ended => "ended",
                }),
            });
            poster
                .post(&protocol::immediate_response(
                    request_id, 200, "Session", data,
                ))
                .await?;
        }
        Ok(None) => {
            poster
                .post(&protocol::error(
                    request_id,
                    "SESSION_NOT_FOUND",
                    &format!("session '{session_id}' not found"),
                ))
                .await?;
        }
        Err(e) => {
            poster
                .post(&protocol::error(
                    request_id,
                    "INTERNAL_ERROR",
                    &format!("get session failed: {e}"),
                ))
                .await?;
        }
    }
    Ok(())
}

/// `send_message` — ack with `immediate_response` (202), run a streaming
/// knowledge-grounded turn over the DynamoDB adapter (reusing the server's
/// runner), forward `stream_token` / `stream_chunk` to the connection as they
/// happen, and finish with `eventual_response` (200).
async fn send_message(
    storage: &Arc<dyn StorageAdapter>,
    config: &LambdaConfig,
    poster: &ConnectionPoster,
    parsed: &Value,
    request_id: Option<&str>,
) -> Result<()> {
    // requestId is load-bearing for streaming correlation; require it.
    let Some(request_id) = request_id else {
        poster
            .post(&protocol::error(
                None,
                "VALIDATION_ERROR",
                "send_message requires a 'requestId'",
            ))
            .await?;
        return Ok(());
    };

    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        poster
            .post(&protocol::error(
                Some(request_id),
                "VALIDATION_ERROR",
                "missing 'sessionId'",
            ))
            .await?;
        return Ok(());
    };

    let message = match parsed.get("message").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => {
            poster
                .post(&protocol::error(
                    Some(request_id),
                    "VALIDATION_ERROR",
                    "missing or empty 'message'",
                ))
                .await?;
            return Ok(());
        }
    };

    let session = match storage.get_session(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            poster
                .post(&protocol::error(
                    Some(request_id),
                    "SESSION_NOT_FOUND",
                    &format!("session '{session_id}' not found"),
                ))
                .await?;
            return Ok(());
        }
        Err(e) => {
            poster
                .post(&protocol::error(
                    Some(request_id),
                    "INTERNAL_ERROR",
                    &format!("get session failed: {e}"),
                ))
                .await?;
            return Ok(());
        }
    };

    // No gateway key → can't run an LLM turn. Clean error; the handler stays
    // usable for protocol-only checks.
    let Some(llm) = config.llm_config() else {
        poster
            .post(&protocol::error(
                Some(request_id),
                "LLM_UNAVAILABLE",
                "SMOOAI_GATEWAY_KEY is not configured; this handler cannot serve LLM turns.",
            ))
            .await?;
        return Ok(());
    };

    // Ack: processing started.
    poster
        .post(&protocol::immediate_response(
            Some(request_id),
            202,
            "Processing your request...",
            json!({}),
        ))
        .await?;

    // Bridge the runner's sink to the Management API: the runner fills `tx`
    // with protocol events; the drain task forwards each to the connection in
    // real time. Run both concurrently.
    let (tx, rx): (UnboundedSender<Value>, UnboundedReceiver<Value>) = mpsc::unbounded_channel();
    let poster_for_drain = poster.clone();
    let drain = tokio::spawn(forward_events(rx, poster_for_drain));

    let result = runner::run_streaming_turn(
        storage.clone(),
        llm,
        config.max_iterations,
        &session.conversation_id,
        request_id,
        &message,
        &tx,
    )
    .await;

    // Closing the sender ends the drain task once it has flushed everything.
    drop(tx);
    let _ = drain.await;

    match result {
        Ok(turn) => {
            let response = runner::general_agent_response(&turn.reply);
            poster
                .post(&protocol::eventual_response(
                    request_id,
                    200,
                    &turn.message_id,
                    response,
                    false,
                    &turn.citations,
                ))
                .await?;
        }
        Err(e) => {
            poster
                .post(&protocol::error(
                    Some(request_id),
                    "AGENT_ERROR",
                    &format!("agent turn failed: {e}"),
                ))
                .await?;
        }
    }
    Ok(())
}

/// Drain the runner's event channel, posting each event to the connection.
/// Stops early (draining the rest without posting) if the client has gone.
async fn forward_events(mut rx: UnboundedReceiver<Value>, poster: ConnectionPoster) {
    let mut connection_open = true;
    while let Some(event) = rx.recv().await {
        if !connection_open {
            // Client gone — keep draining so the sender isn't blocked, but skip
            // the network round-trip for the rest of the turn.
            continue;
        }
        match poster.post(&event).await {
            Ok(true) => {}
            // `Ok(false)` = GoneException; stop posting for the rest of the turn.
            Ok(false) => connection_open = false,
            Err(e) => {
                tracing::warn!(error = %e, "failed to post stream event to connection");
            }
        }
    }
}
