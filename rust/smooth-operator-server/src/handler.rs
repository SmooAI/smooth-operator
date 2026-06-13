//! Action dispatch — parses a client action envelope and produces the matching
//! server events.
//!
//! Each handler is `async` and forwards events through an
//! `UnboundedSender<serde_json::Value>` (the per-connection outbound sink). The
//! socket task drains the sink and writes each value as a JSON WS text frame, so
//! streaming actions (`send_message`) can emit many events while still being
//! driven from one place.

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use smooth_operator::access_control::AccessContext;
use smooth_operator::domain::{
    Conversation, Participant, ParticipantType, Platform, Session, SessionStatus,
};

use crate::protocol;
use crate::runner;
use crate::runner::TurnRequest;
use crate::state::AppState;

/// The agent's display name for the reference server.
const AGENT_NAME: &str = "smooth-agent";

/// Parse and dispatch a single inbound text frame. Any produced events are sent
/// through `sink`. Returns `Ok(())` always — protocol-level failures are
/// surfaced as `error` events, never as hard errors that drop the connection.
pub async fn handle_frame(
    state: &AppState,
    access: &AccessContext,
    conn_id: &str,
    origin: Option<&str>,
    raw: &str,
    sink: &UnboundedSender<Value>,
) {
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            let _ = sink.send(protocol::error(
                None,
                "VALIDATION_ERROR",
                &format!("invalid JSON frame: {e}"),
            ));
            return;
        }
    };

    let action = parsed.get("action").and_then(Value::as_str);
    let request_id = parsed.get("requestId").and_then(Value::as_str);

    match action {
        Some("ping") => {
            let _ = sink.send(protocol::pong(request_id));
        }
        Some("create_conversation_session") => {
            handle_create_session(state, conn_id, origin, &parsed, request_id, sink).await;
        }
        Some("get_session") => {
            handle_get_session(state, &parsed, request_id, sink);
        }
        Some("send_message") => {
            handle_send_message(state, access, &parsed, request_id, sink).await;
        }
        Some(other) => {
            let _ = sink.send(protocol::error(
                request_id,
                "UNSUPPORTED_ACTION",
                &format!("action '{other}' is not supported by this server"),
            ));
        }
        None => {
            let _ = sink.send(protocol::error(
                request_id,
                "VALIDATION_ERROR",
                "missing 'action' field",
            ));
        }
    }
}

/// Enforce an agent's embeddable-widget policy (origin allowlist + `authContext`)
/// before a session is created. Returns `true` to proceed, or `false` after
/// emitting a protocol `error` (the caller must then stop). Agents with no policy
/// proceed — unless `WIDGET_AUTH_STRICT` is set, in which case an unknown agent is
/// rejected (fail closed).
async fn enforce_widget_auth(
    state: &AppState,
    origin: Option<&str>,
    agent_id: &str,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) -> bool {
    let Some(policy) = state.widget_auth.agent_widget_auth(agent_id).await else {
        if state.config.widget_auth_strict {
            let _ = sink.send(protocol::error(
                request_id,
                "AGENT_NOT_AUTHORIZED",
                "this agent is not registered for embedding",
            ));
            return false;
        }
        return true;
    };

    // Origin allowlist — fail closed: a missing or disallowed `Origin` is rejected.
    if !smooth_operator::widget_auth::origin_allowed(
        &policy.allowed_origins,
        origin.unwrap_or_default(),
    ) {
        let _ = sink.send(protocol::error(
            request_id,
            "ORIGIN_NOT_ALLOWED",
            "this origin is not allowed to embed this agent",
        ));
        return false;
    }

    // Pre-auth `authContext` (optional): when present it must verify.
    if let Some(ac) = parsed.get("authContext") {
        if !verify_auth_context_value(policy.public_key.as_deref(), ac) {
            let _ = sink.send(protocol::error(
                request_id,
                "AUTH_CONTEXT_INVALID",
                "authContext signature failed verification",
            ));
            return false;
        }
    }
    true
}

/// Verify a JSON `authContext` (`{userId, signature, timestamp}`) against the
/// agent's `public_key`. False on any missing field/key or signature/replay
/// failure. Replay window: 60s.
fn verify_auth_context_value(public_key: Option<&str>, ac: &Value) -> bool {
    let (Some(pk), Some(user_id), Some(signature), Some(timestamp)) = (
        public_key,
        ac.get("userId").and_then(Value::as_str),
        ac.get("signature").and_then(Value::as_str),
        ac.get("timestamp").and_then(Value::as_i64),
    ) else {
        return false;
    };
    let now = chrono::Utc::now().timestamp();
    smooth_operator::widget_auth::verify_auth_context(pk, user_id, signature, timestamp, now, 60)
}

/// `create_conversation_session` — create a conversation + user & agent
/// participants + a session, then reply with an `immediate_response` carrying
/// the session descriptor (per `create-conversation-session.schema.json`).
async fn handle_create_session(
    state: &AppState,
    conn_id: &str,
    origin: Option<&str>,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    let agent_id = parsed
        .get("agentId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Embeddable-widget auth: enforce the agent's origin allowlist + authContext
    // before creating any session. No-op for agents without a policy (unless
    // WIDGET_AUTH_STRICT). On denial, an error is emitted and we stop here.
    if !enforce_widget_auth(state, origin, &agent_id, parsed, request_id, sink).await {
        return;
    }

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
    // The reference server is single-org; conversations belong to the seed org so
    // the admin API's org-scoping (document sets, indexing runs) lines up with
    // the seeded knowledge. A multi-tenant deployment derives this from auth.
    let org_id = crate::server::SEED_ORG_ID.to_string();

    let conversation_id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();
    let user_participant_id = uuid::Uuid::new_v4().to_string();
    let agent_participant_id = uuid::Uuid::new_v4().to_string();

    // Associate this connection with its session (and agent) on the backplane so
    // events published to the session/agent — by an agent turn or any other
    // service — reach this client's socket, on this pod or (with a Redis/NATS
    // backplane) any pod.
    state
        .backplane
        .associate(
            conn_id,
            smooth_operator::backplane::Target::Session(session_id.clone()),
        )
        .await;
    state
        .backplane
        .associate(
            conn_id,
            smooth_operator::backplane::Target::Agent(agent_id.clone()),
        )
        .await;

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
        // replaying this conversation's persisted message log (see runner.rs).
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

    // Persist to the storage adapter (best-effort: a failure surfaces as error).
    let storage = state.storage.clone();
    let sink_clone = sink.clone();
    let request_id_owned = request_id.map(str::to_string);
    let session_for_registry = session.clone();
    let state_clone = state.clone();

    let data = json!({
        "sessionId": session_id,
        "conversationId": conversation_id,
        "agentId": agent_id,
        "agentName": AGENT_NAME,
        "userParticipantId": user_participant_id,
        "agentParticipantId": agent_participant_id,
    });

    tokio::spawn(async move {
        let rid = request_id_owned.as_deref();
        if let Err(e) = storage.create_conversation(conversation).await {
            let _ = sink_clone.send(protocol::error(
                rid,
                "INTERNAL_ERROR",
                &format!("create conversation failed: {e}"),
            ));
            return;
        }
        if let Err(e) = storage.add_participant(user_participant).await {
            let _ = sink_clone.send(protocol::error(
                rid,
                "INTERNAL_ERROR",
                &format!("add user participant failed: {e}"),
            ));
            return;
        }
        if let Err(e) = storage.add_participant(agent_participant).await {
            let _ = sink_clone.send(protocol::error(
                rid,
                "INTERNAL_ERROR",
                &format!("add agent participant failed: {e}"),
            ));
            return;
        }
        if let Err(e) = storage.create_session(session).await {
            let _ = sink_clone.send(protocol::error(
                rid,
                "INTERNAL_ERROR",
                &format!("create session failed: {e}"),
            ));
            return;
        }
        state_clone.insert_session(session_for_registry);
        let _ = sink_clone.send(protocol::immediate_response(
            rid,
            200,
            "Session created",
            data,
        ));
    });
}

/// `get_session` — return the session snapshot (per `get-session.schema.json`).
fn handle_get_session(
    state: &AppState,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "missing 'sessionId'",
        ));
        return;
    };

    match state.get_session(session_id) {
        Some(s) => {
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
            let _ = sink.send(protocol::immediate_response(
                request_id, 200, "Session", data,
            ));
        }
        None => {
            let _ = sink.send(protocol::error(
                request_id,
                "SESSION_NOT_FOUND",
                &format!("session '{session_id}' not found"),
            ));
        }
    }
}

/// `send_message` — ack with `immediate_response` (202), run a streaming
/// knowledge-grounded turn, emit `stream_token` / `stream_chunk` as it goes, and
/// finish with `eventual_response` (200). Errors (no gateway key, unknown
/// session, agent failure) surface as clean `error` events.
async fn handle_send_message(
    state: &AppState,
    access: &AccessContext,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    // requestId is load-bearing for streaming correlation; require it.
    let Some(request_id) = request_id else {
        let _ = sink.send(protocol::error(
            None,
            "VALIDATION_ERROR",
            "send_message requires a 'requestId'",
        ));
        return;
    };

    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "missing 'sessionId'",
        ));
        return;
    };

    let message = match parsed.get("message").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "VALIDATION_ERROR",
                "missing or empty 'message'",
            ));
            return;
        }
    };

    let Some(session) = state.get_session(session_id) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "SESSION_NOT_FOUND",
            &format!("session '{session_id}' not found"),
        ));
        return;
    };

    // No gateway key → can't run an LLM turn. Return a clean error (the server
    // stays usable for protocol-only checks).
    let Some(llm) = state.config.llm_config() else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "LLM_UNAVAILABLE",
            "SMOOAI_GATEWAY_KEY is not configured; this server cannot serve LLM turns. \
             Set the gateway key to enable send_message.",
        ));
        return;
    };

    // Ack: processing started.
    let _ = sink.send(protocol::immediate_response(
        Some(request_id),
        202,
        "Processing your request...",
        json!({}),
    ));

    let result = runner::run_streaming_turn(
        TurnRequest {
            storage: state.storage.clone(),
            llm,
            max_iterations: state.config.max_iterations,
            conversation_id: &session.conversation_id,
            request_id,
            user_message: &message,
            // The connection's resolved document-level entitlement: retrieval is
            // filtered to what this requester may read (org-public only when the
            // connection is anonymous).
            access: access.clone(),
            llm_provider: None,
            // Opt-in rerank stage (feature gap G8): `None` unless the operator
            // enabled it via `SMOOTH_AGENT_RERANK` (gateway/lexical). Default-off
            // keeps retrieval behavior unchanged.
            reranker: crate::reranker::build_reranker(
                &crate::reranker::RerankerConfig::from_server_config(&state.config),
            ),
        },
        sink,
    )
    .await;

    match result {
        Ok(turn) => {
            let response = runner::general_agent_response(&turn.reply);
            let _ = sink.send(protocol::eventual_response(
                request_id,
                200,
                &turn.message_id,
                response,
                false,
                &turn.citations,
            ));
        }
        Err(e) => {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "AGENT_ERROR",
                &format!("agent turn failed: {e}"),
            ));
        }
    }
}
