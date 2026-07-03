//! Action dispatch — parses a client action envelope and produces the matching
//! server events.
//!
//! Each handler is `async` and forwards events through an
//! `UnboundedSender<serde_json::Value>` (the per-connection outbound sink). The
//! socket task drains the sink and writes each value as a JSON WS text frame, so
//! streaming actions (`send_message`) can emit many events while still being
//! driven from one place.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use smooth_operator::access_control::AccessContext;
use smooth_operator::agent_config::{AgentBehaviorConfig, AuthGateHook, AuthLevel};
use smooth_operator::domain::{
    Conversation, Participant, ParticipantType, Platform, Session, SessionStatus,
};
use smooth_operator::identity_intake::{validate_intake, IntakeOutcome, IntakeValues};
use smooth_operator_core::llm_provider::LlmProvider;
use smooth_operator_core::{LlmClient, LlmConfig};

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
    auth_org: Option<&str>,
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
            handle_create_session(state, conn_id, origin, auth_org, &parsed, request_id, sink)
                .await;
        }
        Some("get_session") => {
            handle_get_session(state, &parsed, request_id, sink);
        }
        Some("get_conversation_messages") => {
            handle_get_conversation_messages(state, &parsed, request_id, sink).await;
        }
        Some("send_message") => {
            handle_send_message(state, access, &parsed, request_id, sink).await;
        }
        Some("confirm_tool_action") => {
            handle_confirm_tool_action(state, &parsed, request_id, sink);
        }
        Some("verify_otp") => {
            handle_verify_otp(state, &parsed, request_id, sink).await;
        }
        Some("submit_identity_intake") => {
            handle_submit_identity_intake(state, &parsed, request_id, sink);
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

/// Outcome of widget-auth enforcement: whether to proceed, and (when an agent
/// policy resolved) the org that policy attributes the agent to.
enum WidgetAuthOutcome {
    /// Auth denied — an `error` event was already emitted; the caller must stop.
    Denied,
    /// Auth passed. `org_id` is `Some` when the resolved policy carried an
    /// `organization_id` (a multi-tenant host that knows the agent's org), else
    /// `None` (no policy, or a policy without an org — org derivation falls
    /// through to the JWT principal, then the seed org).
    Allowed { org_id: Option<String> },
}

/// Enforce an agent's embeddable-widget policy (origin allowlist + `authContext`)
/// before a session is created. Returns [`WidgetAuthOutcome::Allowed`] to proceed
/// (carrying the policy's org when known), or [`WidgetAuthOutcome::Denied`] after
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
) -> WidgetAuthOutcome {
    let Some(policy) = state.widget_auth.agent_widget_auth(agent_id).await else {
        if state.config.widget_auth_strict {
            let _ = sink.send(protocol::error(
                request_id,
                "AGENT_NOT_AUTHORIZED",
                "this agent is not registered for embedding",
            ));
            return WidgetAuthOutcome::Denied;
        }
        return WidgetAuthOutcome::Allowed { org_id: None };
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
        return WidgetAuthOutcome::Denied;
    }

    // Pre-auth `authContext` (optional): when present it must verify.
    if let Some(ac) = parsed.get("authContext") {
        if !verify_auth_context_value(policy.public_key.as_deref(), ac) {
            let _ = sink.send(protocol::error(
                request_id,
                "AUTH_CONTEXT_INVALID",
                "authContext signature failed verification",
            ));
            return WidgetAuthOutcome::Denied;
        }
    }
    WidgetAuthOutcome::Allowed {
        org_id: policy.organization_id,
    }
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
    auth_org: Option<&str>,
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
    // WIDGET_AUTH_STRICT). On denial, an error is emitted and we stop here. A
    // resolved policy may also carry the agent's org (multi-tenant host).
    let widget_org =
        match enforce_widget_auth(state, origin, &agent_id, parsed, request_id, sink).await {
            WidgetAuthOutcome::Denied => return,
            WidgetAuthOutcome::Allowed { org_id } => org_id,
        };

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
    // Derive the org this session (conversation + participants) belongs to, in
    // priority order:
    //   1. the widget policy's `organization_id` — a multi-tenant host that knows
    //      the agent's org (widget visitors authenticate via origin/authContext,
    //      not a JWT, so their org rides on the agent's policy);
    //   2. the connection's authenticated JWT principal org (`auth_org`) — a
    //      dashboard user / authed client;
    //   3. the server's seed org — the single-org reference/dev case, so the
    //      admin API's org-scoping (document sets, indexing runs) still lines up
    //      with the seeded knowledge. This keeps the no-auth/local flavor
    //      behavior unchanged.
    let org_id = widget_org
        .or_else(|| auth_org.map(str::to_string))
        .unwrap_or_else(|| crate::server::SEED_ORG_ID.to_string());

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
        email: user_email.clone(),
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

    // Client render capabilities (`supports`, per
    // create-conversation-session.schema.json). `identity_form` means this
    // client can render the structured identity-intake form, so mid-turn
    // `identity_intake_required` events may be emitted; sessions without it get
    // the conversational intake fallback. Unknown values are ignored.
    let supports_identity_form = parsed
        .get("supports")
        .and_then(Value::as_array)
        .is_some_and(|caps| caps.iter().any(|c| c.as_str() == Some("identity_form")));

    // Stash the caller's OTP contact on the session so the end_user auth-gate
    // flow can offer verification without a storage roundtrip (mirrors how the
    // workflow step pointer lives in session metadata). The reference create path
    // captures only an email; a host that also captures a phone would add
    // `contactPhone` here for an SMS channel. The `identity_form` capability
    // rides the same metadata map.
    let session_metadata = {
        let mut meta = std::collections::HashMap::new();
        if let Some(email) = user_email.as_ref() {
            meta.insert("contactEmail".to_string(), Value::from(email.clone()));
        }
        if supports_identity_form {
            meta.insert("supportsIdentityForm".to_string(), Value::from(true));
        }
        (!meta.is_empty()).then_some(meta)
    };

    let session = Session {
        session_id: session_id.clone(),
        conversation_id: conversation_id.clone(),
        organization_id: org_id.clone(),
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
        metadata: session_metadata,
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

/// `get_conversation_messages` — paginated message history for a session's
/// conversation. Wraps the storage adapter's `list_messages_by_conversation`
/// (the same call the admin API + the turn runner use) and replies with an
/// `immediate_response` carrying `{ conversationId, messages, nextCursor, hasMore }`.
///
/// Optional inputs: `limit` (default 50) and an opaque `cursor` from a prior
/// page's `nextCursor`. Newest-first (the common "recent history" read).
async fn handle_get_conversation_messages(
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
    let Some(session) = state.get_session(session_id) else {
        let _ = sink.send(protocol::error(
            request_id,
            "SESSION_NOT_FOUND",
            &format!("session '{session_id}' not found"),
        ));
        return;
    };

    const DEFAULT_LIMIT: usize = 50;
    let limit = parsed
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIMIT);
    let cursor = parsed
        .get("cursor")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut query = smooth_operator::adapter::MessageQuery::new(&session.conversation_id, limit);
    query.cursor = cursor;
    query.descending = true;

    match state.storage.list_messages_by_conversation(query).await {
        Ok(page) => {
            let data = json!({
                "conversationId": session.conversation_id,
                "messages": page.messages,
                "nextCursor": page.next_cursor,
                "hasMore": page.next_cursor.is_some(),
            });
            let _ = sink.send(protocol::immediate_response(
                request_id,
                200,
                "ConversationMessages",
                data,
            ));
        }
        Err(e) => {
            let _ = sink.send(protocol::error(
                request_id,
                "STORAGE_ERROR",
                &format!("failed to list messages: {e}"),
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

    // A test-injected provider (the scenario-parity corpus's `MockLlmClient`)
    // overrides the live gateway client entirely — the turn never touches the
    // network — so it does NOT need a configured gateway key. Production leaves
    // `chat_provider` `None`, so this clone is `None` and the key gate below is
    // unchanged.
    let chat_provider = state.chat_provider.clone();

    // Resolve the gateway key for this turn's org. The conversation carries the
    // org; the resolver maps it to a per-org key (e.g. a LiteLLM virtual key per
    // tenant) so a multi-tenant flavor bills/scopes each org separately. The
    // default `EnvGatewayKeyResolver` returns the single env key for every org,
    // so the local/default flavor is unchanged. On `None` (no per-org key) we
    // fall back to the env key; only when neither supplies a key do we error.
    let org_id = match state
        .storage
        .get_conversation(&session.conversation_id)
        .await
    {
        Ok(Some(conversation)) => conversation.organization_id,
        // No conversation row (shouldn't happen for a live session) → resolve as
        // if anonymous; the env fallback still applies.
        Ok(None) | Err(_) => String::new(),
    };
    let resolved_key = smooth_operator::gateway_key::resolve_gateway_key(
        &state.gateway_key_resolver,
        &org_id,
        state.config.gateway_key.as_deref(),
    )
    .await;

    // No resolvable key → can't run a *live* LLM turn. Return a clean error (the
    // server stays usable for protocol-only checks). When a mock provider is
    // injected we fall back to a placeholder config — the mock replaces the
    // client built from it, so its url/key/model are never used.
    // Keep a copy of the resolved key to thread into the turn's
    // `ToolProviderContext` (a retrieval-style host tool calls the same gateway);
    // `None` on the mock/placeholder path so a host tool can fall back.
    let turn_gateway_key = resolved_key.clone();
    let llm = match resolved_key {
        Some(key) => state.config.llm_config_with_key(key),
        None if chat_provider.is_some() => state.config.placeholder_llm_config(),
        None => {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "LLM_UNAVAILABLE",
                "No LLM gateway key is available for this turn (SMOOAI_GATEWAY_KEY is unset and no \
                 per-org key resolved); this server cannot serve LLM turns. Configure the gateway \
                 key to enable send_message.",
            ));
            return;
        }
    };

    // Per-turn model override (Smooth Modes / `/smooth-mode` preset): when the
    // send_message body carries a non-empty `model`, run THIS turn on it,
    // overriding the server's configured default model. Absent or blank ⇒ the
    // config default is kept, so behavior is unchanged when the field is unused.
    let llm = apply_model_override(llm, parsed);

    // Ack: processing started.
    let _ = sink.send(protocol::immediate_response(
        Some(request_id),
        202,
        "Processing your request...",
        json!({}),
    ));

    // Run the turn in a spawned task, NOT inline. A turn that calls a
    // confirmation-gated write tool **parks** awaiting a later
    // `confirm_tool_action` frame; the socket reader dispatches that frame on the
    // same connection, so blocking the reader here would deadlock (the confirm
    // can never be read). Spawning frees the reader to receive the confirmation
    // while the turn streams its events through the (cloned) sink. Pearl: HITL
    // pause/resume.
    let confirmation = state.config.confirmation_tool_patterns().map(|patterns| {
        crate::runner::ConfirmationConfig {
            tool_patterns: patterns,
            session_id: session.session_id.clone(),
            register: {
                let state = state.clone();
                Arc::new(move |sid: &str, responder| state.register_confirmation(sid, responder))
            },
            clear: {
                let state = state.clone();
                Arc::new(move |sid: &str| state.clear_confirmation(sid))
            },
        }
    });

    // Identity intake: always wired in the WS server (the primitive is on; the
    // per-agent `enabled_tools` allow-list can restrict it). The form path is
    // selected by the capability the client declared at create-session.
    let identity_intake = Some(crate::runner::IdentityIntakeConfig {
        session_id: session.session_id.clone(),
        form_supported: state.session_supports_identity_form(session_id),
        register: {
            let state = state.clone();
            Arc::new(move |sid: &str, fields, responder| {
                state.register_intake(sid, fields, responder);
            })
        },
        clear: {
            let state = state.clone();
            Arc::new(move |sid: &str| state.clear_intake(sid))
        },
        attach: {
            let state = state.clone();
            let sid = session.session_id.clone();
            Arc::new(move |values| state.attach_session_identity(&sid, values))
        },
    });

    // The reference server is single-org; a multi-tenant host derives this from
    // auth. Used to (a) resolve the org's persona override (SEAM 2) and (b)
    // scope the host's tool provider (SEAM 1).
    let org_id = crate::server::SEED_ORG_ID.to_string();

    // SEAM 3 — per-agent behavior config (instructions + conversation workflow),
    // resolved by the connection's `agent_id` so two agents in the same org can
    // behave differently. Absent / malformed ⇒ `None`, so the org-default persona
    // (SEAM 2) is used, unchanged. Isolated per agent by construction.
    let agent_cfg: Option<AgentBehaviorConfig> =
        state.agent_config.resolve(&session.agent_id).await;

    // SEAM 2/3 — resolve the system prompt in priority order:
    //   1. the per-AGENT instructions (+ personality), when set,
    //   2. the per-ORG persona override ([`AgentSettings::persona`]),
    //   3. the host's installed default persona ([`AppState::default_persona`]).
    // All absent ⇒ `None`, so the runner stays on its const customer-support
    // prompt and behavior is byte-for-byte unchanged.
    let system_prompt = agent_cfg
        .as_ref()
        .and_then(AgentBehaviorConfig::system_prompt)
        .or_else(|| state.settings.get(&org_id).persona)
        .or_else(|| state.default_persona.clone());

    // The agent's first-turn greeting section (the runner injects it only when
    // the conversation has no prior messages) + its tool allow-list (`None` ⇒ the
    // full server tool set).
    let greeting_section = agent_cfg
        .as_ref()
        .and_then(AgentBehaviorConfig::greeting_section);
    let enabled_tools = agent_cfg
        .as_ref()
        .and_then(AgentBehaviorConfig::enabled_tool_ids);

    // Per-tool config delivered to host tools at execution + the authLevel gate.
    let tool_configs = agent_cfg
        .as_ref()
        .map(AgentBehaviorConfig::tool_configs)
        .filter(|m| !m.is_empty());
    // The session's identity-verified bit (set by a prior successful verify_otp)
    // is threaded into the gate so a verified caller's `end_user` tools run.
    let session_authed = state.session_authenticated(session_id);
    let auth_gate = agent_cfg
        .as_ref()
        .and_then(|cfg| build_auth_gate(state, cfg, session_authed));
    // Keep a handle to the gate's OTP-refusal flag so, after the turn, we can see
    // whether an `end_user` tool was refused for lack of verification and (with an
    // OtpService installed + a known contact) offer the OTP flow. `None` when
    // there's no gate — the OTP flow can't trigger.
    let otp_gate = auth_gate.clone();

    // The agent's conversation workflow (if any) + the step this session is on.
    let workflow = agent_cfg
        .as_ref()
        .and_then(|c| c.conversation_workflow.clone())
        .map(|wf| runner::WorkflowTurn {
            workflow: wf,
            current_step_id: state.session_current_step(session_id),
        });

    // The judge LLM surface — only built when there's a workflow to advance. A
    // test-injected chat provider (the mock) doubles as the judge offline; in
    // production the judge runs on the server's default (cheap) model with the
    // turn's resolved gateway key, independent of any per-turn model override so
    // the yes/no/maybe decision stays cheap.
    let judge: Option<Arc<dyn LlmProvider>> = if workflow.is_some() {
        Some(build_judge_provider(state, &llm))
    } else {
        None
    };

    // SEAM 1 — host tool provider (None by default ⇒ built-ins only).
    let tool_provider = state.tool_provider.clone();
    let session_id_owned = session_id.to_string();

    let state_for_turn = state.clone();
    // Carry the turn's org on the AccessContext so a multi-tenant host adapter's
    // `knowledge_for_access` can scope RAG to this tenant. The authed-principal
    // path already stamps its own org (`Principal::access_context`); a widget /
    // anonymous connection does not, so fall back to the session's persisted org
    // (every session carries `organization_id` since the create-session path
    // derives it). The operator's built-in single-tenant ACL ignores the org, so
    // this is behavior-preserving for the reference flavor.
    let access_owned = if access.organization_id.is_some() {
        access.clone()
    } else {
        access
            .clone()
            .with_organization_id(session.organization_id.clone())
    };
    let sink_owned = sink.clone();
    let request_id_owned = request_id.to_string();
    let conversation_id = session.conversation_id.clone();

    tokio::spawn(async move {
        // SEP — build this turn's extension host (only when SMOOTH_EXTENSIONS_ALLOW
        // is set; `None` otherwise, zero overhead). The delegate is bound to THIS
        // turn's sink/request/session so a hosted extension's `ui/confirm` routes
        // back over this connection.
        let extensions = crate::extensions::build_extension_host(
            &state_for_turn,
            &session_id_owned,
            &request_id_owned,
            sink_owned.clone(),
        )
        .await;
        let result = runner::run_streaming_turn(
            TurnRequest {
                storage: state_for_turn.storage.clone(),
                llm,
                max_iterations: state_for_turn.config.max_iterations,
                conversation_id: &conversation_id,
                request_id: &request_id_owned,
                user_message: &message,
                // The connection's resolved document-level entitlement: retrieval is
                // filtered to what this requester may read (org-public only when the
                // connection is anonymous).
                access: access_owned,
                // Production: `None` (a live client is built from `llm`). Tests: the
                // scenario corpus's `MockLlmClient`, which runs the turn offline.
                llm_provider: chat_provider,
                // Opt-in rerank stage (feature gap G8): `None` unless the operator
                // enabled it via `SMOOTH_AGENT_RERANK` (gateway/lexical). Default-off
                // keeps retrieval behavior unchanged.
                reranker: crate::reranker::build_reranker(
                    &crate::reranker::RerankerConfig::from_server_config(&state_for_turn.config),
                ),
                confirmation,
                // Channel-normalized identity intake (form park on capable
                // clients; conversational fallback otherwise).
                identity_intake,
                // SEAM 1 — host tool provider (None by default ⇒ built-ins only).
                tool_provider,
                // SEAM 2 — resolved per-org persona (None ⇒ const prompt).
                system_prompt,
                org_id: Some(org_id),
                // The per-org key resolved above, threaded so a host tool
                // provider's retrieval tools call the same gateway this turn used.
                gateway_key: turn_gateway_key,
                // SEAM 3 — per-agent conversation workflow + its cheap judge. Both
                // `None` for a freeform agent, so the turn is unchanged.
                workflow,
                judge,
                // SEAM 3 — per-agent first-turn greeting + tool allow-list.
                greeting_section,
                enabled_tools,
                // SEAM 3 — authLevel gate + per-tool config delivery.
                auth_gate,
                tool_configs,
                // SEP — the per-turn extension host (None unless allowlisted).
                extensions,
            },
            &sink_owned,
        )
        .await;

        match result {
            Ok(turn) => {
                // Persist the workflow step pointer the judge landed on, so the
                // next turn resumes on the right step. No-op when the agent has no
                // workflow (`next_step_id` is `None`).
                if let Some(step) = turn.next_step_id.as_deref() {
                    state_for_turn.set_session_current_step(&session_id_owned, Some(step));
                }
                // If the auth gate refused an `end_user` tool for lack of a
                // verified session this turn, and a host OTP service is installed
                // and the session has a contact to reach, offer the OTP flow
                // (prompt → dispatch → ack). The reference server does not
                // park/auto-resume; the client verifies via `verify_otp` and
                // re-sends its message once the session is authenticated.
                if let (Some(gate), Some(otp)) =
                    (otp_gate.as_ref(), state_for_turn.otp_service.clone())
                {
                    if let Some(tool) = gate.otp_refused_tool() {
                        let contact = state_for_turn.session_contact(&session_id_owned);
                        if !contact.is_empty() {
                            offer_otp(
                                otp.as_ref(),
                                &session_id_owned,
                                &tool,
                                &contact,
                                &request_id_owned,
                                &sink_owned,
                            )
                            .await;
                        }
                    }
                }
                let response = runner::general_agent_response(&turn.reply);
                let _ = sink_owned.send(protocol::eventual_response(
                    &request_id_owned,
                    200,
                    &turn.message_id,
                    response,
                    false,
                    &turn.citations,
                    turn.usage,
                ));
            }
            Err(e) => {
                let _ = sink_owned.send(protocol::error(
                    Some(&request_id_owned),
                    "AGENT_ERROR",
                    &format!("agent turn failed: {e}"),
                ));
            }
        }
    });
}

/// `confirm_tool_action` — resume a turn parked on a write-tool confirmation.
///
/// Per `spec/actions/confirm-tool-action.schema.json` the client sends
/// `{ action, sessionId, requestId, approved }` in reply to a
/// `write_confirmation_required` event. We look up the session's registered
/// [`HumanResponse`](smooth_operator_core::HumanResponse) sender (set by the
/// runner's confirmation bridge when the turn parked), take it, and feed it the
/// verdict: `approved` → `Approved` (the parked tool executes), else `Denied`
/// (the tool is skipped with a rejection result the model sees). There is no
/// dedicated response event — the resumed workflow signals continuation via its
/// normal streaming sequence (`stream_chunk`/`stream_token` → `eventual_response`);
/// we additionally ack with an `immediate_response`. Taking the sender makes a
/// duplicate confirm a no-op (`NO_PENDING_CONFIRMATION`).
fn handle_confirm_tool_action(
    state: &AppState,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "confirm_tool_action requires a 'sessionId'",
        ));
        return;
    };

    // `approved` is required and must be a boolean — a missing/garbled verdict
    // must NOT silently approve a write. Fail closed on a bad shape.
    let Some(approved) = parsed.get("approved").and_then(Value::as_bool) else {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "confirm_tool_action requires a boolean 'approved'",
        ));
        return;
    };

    let Some(responder) = state.take_confirmation(session_id) else {
        let _ = sink.send(protocol::error(
            request_id,
            "NO_PENDING_CONFIRMATION",
            &format!("no tool action is awaiting confirmation for session '{session_id}'"),
        ));
        return;
    };

    let verdict = if approved {
        smooth_operator_core::HumanResponse::Approved
    } else {
        smooth_operator_core::HumanResponse::Denied {
            reason: "user rejected the action".to_string(),
        }
    };

    if responder.send(verdict).is_err() {
        // The parked turn ended (timeout / disconnect) before the confirm landed.
        let _ = sink.send(protocol::error(
            request_id,
            "NO_PENDING_CONFIRMATION",
            &format!(
                "the turn awaiting confirmation for session '{session_id}' is no longer active"
            ),
        ));
        return;
    }

    // Ack the confirmation; the resumed turn streams its own follow-on events.
    let _ = sink.send(protocol::immediate_response(
        request_id,
        200,
        if approved {
            "Tool action approved"
        } else {
            "Tool action rejected"
        },
        json!({ "sessionId": session_id, "approved": approved }),
    ));
}

/// Apply an optional per-turn `model` override (from a `send_message` body) to a
/// resolved [`LlmConfig`]. When the body carries a non-empty `model` string, this
/// turn runs on that gateway model id (a Smooth Modes / `/smooth-mode` preset),
/// overriding the server's configured default; an absent, non-string, or
/// blank/whitespace-only `model` leaves the config's default model unchanged
/// (byte-for-byte the prior behavior). Every other field (url, key, limits)
/// stays as resolved — only the model id changes.
fn apply_model_override(mut llm: LlmConfig, body: &Value) -> LlmConfig {
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        let model = model.trim();
        if !model.is_empty() {
            llm.model = model.to_string();
        }
    }
    llm
}

/// Cap the judge's output: a `yes` / `no` / `maybe` verdict needs only a few
/// tokens. Small so the extra per-turn cost + latency stay negligible.
const JUDGE_MAX_TOKENS: u32 = 16;

/// Build the per-agent authLevel gate, or `None` when it would be inert.
///
/// The set of tools that "support auth requirements" (the operator analog of the
/// TS `supportsAuthRequirement` flag) comes from `SMOOTH_AGENT_AUTH_TOOLS`
/// (comma-separated); empty (the default) ⇒ nothing is gated.
/// `session_authenticated` is the session's OTP-verified bit (from a prior
/// successful `verify_otp`): `false` fail-closed-refuses `end_user` tools (and,
/// with an OtpService installed, triggers the OTP-offer flow); `true` lets a
/// verified caller's `end_user` tools run.
fn build_auth_gate(
    state: &AppState,
    cfg: &AgentBehaviorConfig,
    session_authenticated: bool,
) -> Option<AuthGateHook> {
    let supporting: std::collections::HashSet<String> = std::env::var("SMOOTH_AGENT_AUTH_TOOLS")
        .ok()
        .into_iter()
        .flat_map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    if supporting.is_empty() {
        let _ = state; // no host-declared auth-supporting tools → gate is inert
        return None;
    }
    let levels = cfg
        .enabled_tools
        .iter()
        .map(|t| (t.tool_id.clone(), AuthLevel::parse(&t.auth_level)))
        .collect();
    let hook = AuthGateHook::new(levels, cfg.visibility, session_authenticated, supporting);
    hook.is_active().then_some(hook)
}

/// Emit the OTP-offer sequence for a turn whose `end_user` tool was refused for
/// lack of a verified session: `otp_verification_required` (prompt the client),
/// then `send_otp` on the host service, then `otp_sent` (ack delivery) — or an
/// `error` event if delivery fails. The masked destination + channel come from
/// the host; the server never sees the code. `auth_level` is fixed `end_user`
/// (the only level this flow remedies).
async fn offer_otp(
    otp: &dyn smooth_operator::otp::OtpService,
    session_id: &str,
    tool: &str,
    contact: &smooth_operator::otp::OtpContact,
    request_id: &str,
    sink: &UnboundedSender<Value>,
) {
    let channels: Vec<&str> = contact
        .available_channels()
        .iter()
        .map(|c| c.as_str())
        .collect();
    let _ = sink.send(protocol::otp_verification_required(
        request_id,
        tool,
        &format!("Verify your identity to continue using '{tool}'."),
        &channels,
        "end_user",
    ));
    match otp.send_otp(session_id, contact).await {
        Ok(delivery) => {
            let _ = sink.send(protocol::otp_sent(
                request_id,
                delivery.channel.as_str(),
                &delivery.masked_destination,
            ));
        }
        Err(e) => {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "OTP_SEND_FAILED",
                &format!("failed to send verification code: {e}"),
            ));
        }
    }
}

/// `submit_identity_intake` — resume a turn parked on identity intake.
///
/// Per `spec/actions/submit-identity-intake.schema.json` the client sends
/// `{ action, sessionId, requestId, values?, declined? }` in reply to an
/// `identity_intake_required` event. Validation is **server-side** against the
/// fields the parked `request_identity_intake` asked for (required-ness, email
/// shape, E.164 phone):
///   - invalid → an `identity_intake_invalid` event with per-field errors; the
///     turn STAYS parked so the form can resubmit (mirrors `otp_invalid`);
///   - valid → the identity is attached to the session (metadata `userName` /
///     `contactEmail` / `contactPhone` — the OTP contact keys), the parked tool
///     resumes with the validated payload, and an `immediate_response` acks;
///   - `declined: true` → the tool resumes with a declined payload (the agent
///     handles it gracefully).
///
/// Taking the responder only on resolution makes a duplicate submit a no-op
/// (`NO_PENDING_INTAKE`).
fn handle_submit_identity_intake(
    state: &AppState,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    // requestId is load-bearing (it echoes the originating
    // identity_intake_required); require it.
    let Some(request_id) = request_id else {
        let _ = sink.send(protocol::error(
            None,
            "VALIDATION_ERROR",
            "submit_identity_intake requires a 'requestId'",
        ));
        return;
    };

    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "submit_identity_intake requires a 'sessionId'",
        ));
        return;
    };

    // Peek the pending intake's fields WITHOUT consuming the park — an invalid
    // submit must leave the turn parked for a resubmit.
    let Some(fields) = state.intake_fields(session_id) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "NO_PENDING_INTAKE",
            &format!("no identity intake is awaiting submission for session '{session_id}'"),
        ));
        return;
    };

    // Decline path: resume the tool with a declined payload.
    if parsed
        .get("declined")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if resolve_intake(state, session_id, request_id, IntakeOutcome::Declined, sink) {
            let _ = sink.send(protocol::immediate_response(
                Some(request_id),
                200,
                "Identity intake declined",
                json!({ "sessionId": session_id, "declined": true }),
            ));
        }
        return;
    }

    // Values path: parse + validate server-side.
    let values: IntakeValues = match parsed.get("values") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(values) => values,
            Err(e) => {
                let _ = sink.send(protocol::error(
                    Some(request_id),
                    "VALIDATION_ERROR",
                    &format!("invalid 'values' shape: {e}"),
                ));
                return;
            }
        },
        None => {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "VALIDATION_ERROR",
                "submit_identity_intake requires 'values' (or 'declined': true)",
            ));
            return;
        }
    };

    match validate_intake(&fields, &values) {
        Err(errors) => {
            // Retryable: the turn stays parked; the client re-renders the form
            // with the per-field errors (never a terminal `error` event).
            let _ = sink.send(protocol::identity_intake_invalid(
                request_id,
                &errors,
                "Some fields need attention.",
            ));
        }
        Ok(validated) => {
            // Attach the identity to the session (same keys the pre-chat /
            // create path stashes; feeds the OTP contact seam), then resume.
            state.attach_session_identity(session_id, &validated);
            if resolve_intake(
                state,
                session_id,
                request_id,
                IntakeOutcome::Submitted(validated.clone()),
                sink,
            ) {
                let _ = sink.send(protocol::immediate_response(
                    Some(request_id),
                    200,
                    "Identity intake submitted",
                    json!({ "sessionId": session_id, "values": validated }),
                ));
            }
        }
    }
}

/// Take the pending intake responder for `session_id` and feed it `outcome`.
/// Returns `true` when the parked turn was resumed; emits `NO_PENDING_INTAKE`
/// and returns `false` when the park raced away (duplicate submit, or the
/// parked turn ended before the submit landed).
fn resolve_intake(
    state: &AppState,
    session_id: &str,
    request_id: &str,
    outcome: IntakeOutcome,
    sink: &UnboundedSender<Value>,
) -> bool {
    let Some(responder) = state.take_intake(session_id) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "NO_PENDING_INTAKE",
            &format!("no identity intake is awaiting submission for session '{session_id}'"),
        ));
        return false;
    };
    if responder.send(outcome).is_err() {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "NO_PENDING_INTAKE",
            &format!("the turn awaiting intake for session '{session_id}' is no longer active"),
        ));
        return false;
    }
    true
}

/// `verify_otp` — validate a submitted OTP code and, on success, mark the
/// session identity-verified. Per `spec/actions/verify-otp.schema.json` the
/// client sends `{ action, sessionId, requestId, code }` in reply to an
/// `otp_verification_required` event. There is no dedicated response event: a
/// correct code emits `otp_verified` (the client then re-sends its message to
/// run the gated tool — the reference server does not park/auto-resume the
/// original turn), a rejected code emits `otp_invalid` carrying the host's
/// remaining-attempt count. With no [`OtpService`](smooth_operator::otp::OtpService)
/// installed, verification is impossible, so we fail closed with an `otp_invalid`
/// (`NOT_FOUND`, 0 attempts).
async fn handle_verify_otp(
    state: &AppState,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    // requestId is load-bearing (it echoes the originating
    // otp_verification_required); require it.
    let Some(request_id) = request_id else {
        let _ = sink.send(protocol::error(
            None,
            "VALIDATION_ERROR",
            "verify_otp requires a 'requestId'",
        ));
        return;
    };

    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "verify_otp requires a 'sessionId'",
        ));
        return;
    };

    let Some(code) = parsed.get("code").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "verify_otp requires a 'code'",
        ));
        return;
    };

    // The session must exist (a code can't verify a session we don't track).
    if state.get_session(session_id).is_none() {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "SESSION_NOT_FOUND",
            &format!("session '{session_id}' not found"),
        ));
        return;
    }

    // No host OTP service → verification is impossible. Fail closed on the
    // documented otp_invalid path (a client shouldn't reach here without first
    // receiving otp_verification_required, which only an installed service emits).
    let Some(otp) = state.otp_service.clone() else {
        let _ = sink.send(protocol::otp_invalid(
            request_id,
            Some("NOT_FOUND"),
            0,
            "No verification is in progress for this session.",
        ));
        return;
    };

    match otp.verify_otp(session_id, code).await {
        smooth_operator::otp::OtpVerifyOutcome::Verified => {
            state.set_session_authenticated(session_id, true);
            let _ = sink.send(protocol::otp_verified(
                request_id,
                "Identity verified successfully.",
            ));
        }
        smooth_operator::otp::OtpVerifyOutcome::Invalid {
            attempts_remaining,
            error,
            message,
        } => {
            let _ = sink.send(protocol::otp_invalid(
                request_id,
                error.map(smooth_operator::otp::OtpError::as_str),
                attempts_remaining,
                &message,
            ));
        }
    }
}

/// Build the workflow judge's LLM surface. Prefers a test-injected chat provider
/// (the scenario mock — runs offline); otherwise builds a live client on the
/// server's **default** (cheap) model with the turn's resolved gateway url/key,
/// independent of any per-turn model override, so the judge stays cheap even when
/// the turn itself runs on a bigger model.
fn build_judge_provider(state: &AppState, turn_llm: &LlmConfig) -> Arc<dyn LlmProvider> {
    if let Some(mock) = state.chat_provider.clone() {
        return mock;
    }
    let mut cfg = turn_llm.clone();
    cfg.model = state.config.judge_model.clone();
    cfg.max_tokens = JUDGE_MAX_TOKENS;
    Arc::new(LlmClient::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smooth_operator_core::llm::{ApiFormat, RetryPolicy};

    /// A baseline config whose `model` is the server default, so each override
    /// test asserts against a known starting model.
    fn base_llm() -> LlmConfig {
        LlmConfig {
            api_url: "https://llm.smoo.ai/v1".to_string(),
            api_key: "sk-test".to_string(),
            model: "claude-haiku-4-5".to_string(),
            max_tokens: 512,
            temperature: 0.0,
            retry_policy: RetryPolicy::default(),
            api_format: ApiFormat::OpenAiCompat,
        }
    }

    #[test]
    fn model_override_present_replaces_model() {
        let body = json!({ "action": "send_message", "model": "claude-opus-4-8" });
        let llm = apply_model_override(base_llm(), &body);
        assert_eq!(llm.model, "claude-opus-4-8");
        // Only the model id changes — every other field is preserved.
        assert_eq!(llm.api_url, "https://llm.smoo.ai/v1");
        assert_eq!(llm.api_key, "sk-test");
        assert_eq!(llm.max_tokens, 512);
    }

    #[test]
    fn model_override_absent_keeps_default() {
        let body = json!({ "action": "send_message", "message": "hi" });
        let llm = apply_model_override(base_llm(), &body);
        assert_eq!(llm.model, "claude-haiku-4-5");
    }

    #[test]
    fn model_override_blank_or_non_string_keeps_default() {
        // Whitespace-only is treated as absent.
        let blank = json!({ "model": "   " });
        assert_eq!(
            apply_model_override(base_llm(), &blank).model,
            "claude-haiku-4-5"
        );
        // A non-string `model` is ignored (no panic, default kept).
        let wrong_type = json!({ "model": 42 });
        assert_eq!(
            apply_model_override(base_llm(), &wrong_type).model,
            "claude-haiku-4-5"
        );
    }
}
