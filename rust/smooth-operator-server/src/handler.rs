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
use smooth_operator::adapter::ConversationUpdate;
use smooth_operator::agent_config::{AgentBehaviorConfig, AuthGateHook, AuthLevel};
use smooth_operator::domain::{
    Conversation, Participant, ParticipantType, Platform, Session, SessionStatus,
};
use smooth_operator::identity_intake::IntakeValues;
use smooth_operator::interaction::InteractionOutcome;
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
        Some("list_conversations") => {
            handle_list_conversations(state, auth_org, &parsed, request_id, sink).await;
        }
        Some("rename_conversation") => {
            handle_rename_conversation(state, &parsed, request_id, sink).await;
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
        Some("submit_interaction") => {
            handle_submit_interaction(state, &parsed, request_id, sink);
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

    // Resume: when the caller passes a `conversationId` for a conversation that
    // exists, bind this new session to it (reuse its id + org, skip
    // `create_conversation`) so subsequent `send_message` appends to it and the
    // runner replays its history by `thread_id`. Absent/unknown id → mint a fresh
    // conversation (byte-for-byte unchanged behavior).
    let resume = match parsed.get("conversationId").and_then(Value::as_str) {
        Some(cid) if !cid.is_empty() => state.storage.get_conversation(cid).await.ok().flatten(),
        _ => None,
    };

    // Derive the org this session (conversation + participants) belongs to. When
    // resuming, it's the existing conversation's org (keeps the session
    // self-consistent). Otherwise, in priority order:
    //   1. the widget policy's `organization_id` — a multi-tenant host that knows
    //      the agent's org (widget visitors authenticate via origin/authContext,
    //      not a JWT, so their org rides on the agent's policy);
    //   2. the connection's authenticated JWT principal org (`auth_org`) — a
    //      dashboard user / authed client;
    //   3. the server's seed org — the single-org reference/dev case, so the
    //      admin API's org-scoping (document sets, indexing runs) still lines up
    //      with the seeded knowledge. This keeps the no-auth/local flavor
    //      behavior unchanged.
    let org_id = if let Some(ref c) = resume {
        c.organization_id.clone()
    } else {
        widget_org
            .or_else(|| auth_org.map(str::to_string))
            .unwrap_or_else(|| crate::server::SEED_ORG_ID.to_string())
    };

    let conversation_id = resume
        .as_ref()
        .map(|c| c.id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
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

    // Only mint a conversation on a fresh session — a resume reuses the existing
    // one (and its persisted history), so `create_conversation` is skipped.
    let conversation = resume.is_none().then(|| Conversation {
        id: conversation_id.clone(),
        platform: Platform::Web,
        name: format!("Session {session_id}"),
        organization_id: org_id.clone(),
        idempotency_key: session_id.clone(),
        metadata_json: parsed.get("metadata").cloned(),
        analytics_json: None,
        created_at: now,
        updated_at: now,
    });

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
    // create-conversation-session.schema.json) — the per-kind list gating which
    // Rich Interactions this session gets as parked cards (e.g. `identity_form`
    // for kind `identity_intake`); kinds without their capability degrade to
    // the conversational fallback. Unknown values are kept (forward-compatible:
    // a future kind may gate on them).
    let supports: Vec<String> = parsed
        .get("supports")
        .and_then(Value::as_array)
        .map(|caps| {
            caps.iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // Stash the caller's OTP contact on the session so the end_user auth-gate
    // flow can offer verification without a storage roundtrip (mirrors how the
    // workflow step pointer lives in session metadata). The reference create path
    // captures only an email; a host that also captures a phone would add
    // `contactPhone` here for an SMS channel. The declared render capabilities
    // (`supports`) ride the same metadata map.
    let session_metadata = {
        let mut meta = std::collections::HashMap::new();
        if let Some(email) = user_email.as_ref() {
            meta.insert("contactEmail".to_string(), Value::from(email.clone()));
        }
        if !supports.is_empty() {
            meta.insert("supports".to_string(), Value::from(supports.clone()));
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
        if let Some(conversation) = conversation {
            if let Err(e) = storage.create_conversation(conversation).await {
                let _ = sink_clone.send(protocol::error(
                    rid,
                    "INTERNAL_ERROR",
                    &format!("create conversation failed: {e}"),
                ));
                return;
            }
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

/// `list_conversations` — the conversation-sidebar / resume substrate. Returns
/// the org's conversations that have at least one message, most-recent-first,
/// each with a short title preview + message count. Empty conversations (every
/// page-load currently mints one) are filtered out so the sidebar isn't buried
/// in blanks. Reply is an `immediate_response` carrying `{ conversations: [ {
/// conversationId, title, updatedAt, messageCount } ] }`.
///
/// Optional input: `limit` (default 50) — the max conversations returned after
/// filtering + sorting.
async fn handle_list_conversations(
    state: &AppState,
    auth_org: Option<&str>,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    const DEFAULT_LIMIT: usize = 50;
    let limit = parsed
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIMIT);

    // Org scope: the authenticated principal's org, else the seed org — matching
    // the create-session derivation's fallback for the local/no-auth flavor.
    let org_id = auth_org.unwrap_or(crate::server::SEED_ORG_ID);

    let conversations = match state.storage.list_conversations_by_org(org_id).await {
        Ok(c) => c,
        Err(e) => {
            let _ = sink.send(protocol::error(
                request_id,
                "STORAGE_ERROR",
                &format!("failed to list conversations: {e}"),
            ));
            return;
        }
    };

    // Peek each conversation's messages for a preview + count, dropping empties.
    // ponytail: per-conversation peek capped at MSG_CAP — fine for a local
    // daemon's ~100 convos. If this ever fronts a multi-thousand-conversation
    // org, push count + first-inbound down into the storage adapter as one query.
    const MSG_CAP: usize = 200;
    let mut rows: Vec<(i64, Value)> = Vec::new();
    for conv in conversations {
        let mut query = smooth_operator::adapter::MessageQuery::new(&conv.id, MSG_CAP);
        query.descending = false; // oldest-first: the first inbound is the title source
        let Ok(page) = state.storage.list_messages_by_conversation(query).await else {
            continue;
        };
        if page.messages.is_empty() {
            continue;
        }
        rows.push((
            conv.updated_at.timestamp_millis(),
            json!({
                "conversationId": conv.id,
                "title": conversation_title(&page.messages, &conv.name),
                "updatedAt": conv.updated_at.to_rfc3339(),
                "messageCount": page.messages.len(),
            }),
        ));
    }

    // Most-recent-first, then cap.
    rows.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    let conversations: Vec<Value> = rows.into_iter().take(limit).map(|(_, v)| v).collect();

    let _ = sink.send(protocol::immediate_response(
        request_id,
        200,
        "Conversations",
        json!({ "conversations": conversations }),
    ));
}

/// Derive a sidebar title. A **meaningful** conversation `name` — an auto-title
/// or a manual rename, i.e. anything not the default `Session <uuid>` — wins, so
/// titles set by [`maybe_auto_title`] / [`handle_rename_conversation`] surface in
/// the sidebar. Otherwise fall back to a truncated preview of the FIRST inbound
/// (user) message, then the default name. `messages` is oldest-first.
///
/// Back-compat: every pre-titling conversation carried the default name, so this
/// is byte-for-byte the old message-preview behavior for them.
fn conversation_title(messages: &[smooth_operator::domain::Message], name: &str) -> String {
    if !name.starts_with(DEFAULT_NAME_PREFIX) && !name.trim().is_empty() {
        return truncate_preview(name, TITLE_MAX);
    }
    messages
        .iter()
        .find(|m| matches!(m.direction, smooth_operator::domain::Direction::Inbound))
        .and_then(message_text)
        .map_or_else(|| name.to_string(), |t| truncate_preview(&t, TITLE_MAX))
}

/// Flat text of a message: the content's `text` mirror, else the first text item.
/// `None` when blank.
fn message_text(m: &smooth_operator::domain::Message) -> Option<String> {
    m.content
        .text
        .clone()
        .or_else(|| m.content.items.iter().find_map(|i| i.text.clone()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Truncate to `max` characters (char-safe), appending `…` when clipped.
fn truncate_preview(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let clipped: String = s.chars().take(max).collect();
    format!("{}…", clipped.trim_end())
}

/// The default conversation name minted at create-session time
/// (`Session <uuid>`). The auto-titler only fires while a conversation still
/// carries this prefix, so a manual rename (or a prior successful auto-title) is
/// never clobbered.
const DEFAULT_NAME_PREFIX: &str = "Session ";

/// Fast/cheap model used to auto-title a conversation from its first exchange.
const AUTO_TITLE_MODEL: &str = "groq-gpt-oss-20b";

/// Max characters of a title (both auto-generated and manually set).
const TITLE_MAX: usize = 60;

/// `rename_conversation` — set a conversation's `name` to a caller-supplied
/// title. Per the daemon-sidebar rename affordance the client sends
/// `{ action, requestId, conversationId, title }`. The title is sanitized/trimmed
/// and rejected when empty; on success the conversation row's `name` is persisted
/// (which `list_conversations` surfaces as the sidebar title, since it prefers
/// `name` over the first-message preview). Replies with an `immediate_response`
/// (200) carrying `{ conversationId, title }`.
async fn handle_rename_conversation(
    state: &AppState,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    let Some(conversation_id) = parsed.get("conversationId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "rename_conversation requires a 'conversationId'",
        ));
        return;
    };

    let title = sanitize_title(
        parsed
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if title.is_empty() {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "rename_conversation requires a non-empty 'title'",
        ));
        return;
    }

    // The conversation must exist — give a clean 404 rather than a generic
    // storage error.
    match state.storage.get_conversation(conversation_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = sink.send(protocol::error(
                request_id,
                "CONVERSATION_NOT_FOUND",
                &format!("conversation '{conversation_id}' not found"),
            ));
            return;
        }
        Err(e) => {
            let _ = sink.send(protocol::error(
                request_id,
                "STORAGE_ERROR",
                &format!("failed to load conversation: {e}"),
            ));
            return;
        }
    }

    let update = ConversationUpdate {
        name: Some(title.clone()),
        ..Default::default()
    };
    match state
        .storage
        .update_conversation(conversation_id, update)
        .await
    {
        Ok(_) => {
            let _ = sink.send(protocol::immediate_response(
                request_id,
                200,
                "Conversation renamed",
                json!({ "conversationId": conversation_id, "title": title }),
            ));
        }
        Err(e) => {
            let _ = sink.send(protocol::error(
                request_id,
                "STORAGE_ERROR",
                &format!("failed to rename conversation: {e}"),
            ));
        }
    }
}

/// Best-effort auto-title: after the first assistant turn on a conversation
/// still carrying the default `Session <uuid>` name, ask a small/cheap model for
/// a short title and persist it as the conversation `name`. **Non-blocking &
/// fail-safe** — spawned detached off the turn path and any failure (no key,
/// gateway error, empty output, storage error) simply leaves the default name.
/// The default-name guard means a manual [`handle_rename_conversation`] rename is
/// never overwritten, and once a title lands the conversation is no longer
/// default-named so it won't re-fire.
pub async fn maybe_auto_title(
    state: &AppState,
    conversation_id: &str,
    user_message: &str,
    reply: &str,
) {
    // Only title conversations still on their default name.
    let conversation = match state.storage.get_conversation(conversation_id).await {
        Ok(Some(c)) => c,
        _ => {
            tracing::warn!(conversation_id, "auto-title: conversation not found");
            return;
        }
    };
    if !conversation.name.starts_with(DEFAULT_NAME_PREFIX) {
        // Expected on every turn after the first (or after a manual rename) — debug.
        tracing::debug!(conversation_id, name = %conversation.name, "auto-title: name not default, skip");
        return;
    }

    // Resolve the org's gateway key (per-org resolver, else the env key). No key
    // ⇒ no title (same gate as a live turn).
    let key = smooth_operator::gateway_key::resolve_gateway_key(
        &state.gateway_key_resolver,
        &conversation.organization_id,
        state.config.gateway_key.as_deref(),
    )
    .await;
    let Some(key) = key else {
        tracing::warn!(org = %conversation.organization_id, "auto-title: no gateway key resolved");
        return;
    };

    let Some(raw) = generate_title(&state.config.gateway_url, &key, user_message, reply).await
    else {
        tracing::warn!("auto-title: generate_title returned None (gateway/parse)");
        return;
    };
    let title = sanitize_title(&raw);
    if title.is_empty() {
        tracing::warn!(raw = %raw, "auto-title: sanitized title empty");
        return;
    }
    tracing::debug!(conversation_id, title = %title, "auto-title: writing title");

    // Re-check the guard right before writing: a manual rename could have landed
    // while the model was thinking. Best-effort — a lost race just means the
    // manual name wins, which is the desired precedence.
    if let Ok(Some(c)) = state.storage.get_conversation(conversation_id).await {
        if !c.name.starts_with(DEFAULT_NAME_PREFIX) {
            return;
        }
    }
    let update = ConversationUpdate {
        name: Some(title),
        ..Default::default()
    };
    let _ = state
        .storage
        .update_conversation(conversation_id, update)
        .await;
}

/// The title model (`groq-gpt-oss-20b`) is a reasoning model: reasoning tokens
/// count against `max_tokens`, so a tight cap (the original 32) gets fully
/// consumed by reasoning and leaves the content empty — the auto-titler then
/// silently produced nothing. Give reasoning headroom; the title itself is
/// capped to `TITLE_MAX` chars by [`sanitize_title`] regardless.
const AUTO_TITLE_MAX_TOKENS: u32 = 512;

/// Build the `/chat/completions` request body for the auto-titler. Extracted so
/// the token budget (the thing that broke) is unit-testable without a gateway.
fn title_request_body(user_message: &str, reply: &str) -> Value {
    let user_snippet: String = user_message.chars().take(500).collect();
    let reply_snippet: String = reply.chars().take(500).collect();
    let prompt = format!(
        "Give this conversation a short 3-6 word title. Reply with ONLY the title, no quotes.\n\nUser: {user_snippet}\nAssistant: {reply_snippet}"
    );
    json!({
        "max_tokens": AUTO_TITLE_MAX_TOKENS,
        "model": AUTO_TITLE_MODEL,
        "temperature": 0.3,
        "messages": [{ "role": "user", "content": prompt }],
    })
}

/// Call the gateway's `/chat/completions` with the small title model over the
/// first exchange, returning the model's raw title text (unsanitized). `None` on
/// any transport / non-2xx / decode failure or a missing content field — the
/// caller treats that as "no title". Inputs are truncated so the prompt stays
/// cheap regardless of how long the exchange ran.
async fn generate_title(
    gateway_url: &str,
    key: &str,
    user_message: &str,
    reply: &str,
) -> Option<String> {
    let url = format!("{}/chat/completions", gateway_url.trim_end_matches('/'));
    let resp: Value = reqwest::Client::new()
        .post(&url)
        .bearer_auth(key)
        .json(&title_request_body(user_message, reply))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    resp.get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()
        .map(str::to_string)
}

/// Sanitize a conversation title (auto-generated or manually supplied): collapse
/// all whitespace/newlines to single spaces, strip wrapping quotes / markdown
/// emphasis the model sometimes adds (`"`, `'`, `*`, `` ` ``, `#`), and cap at
/// [`TITLE_MAX`] chars (char-safe). Returns an empty string for blank/whitespace
/// input so callers can reject it.
fn sanitize_title(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| matches!(c, '"' | '\'' | '*' | '`' | '#' | ' '));
    trimmed
        .chars()
        .take(TITLE_MAX)
        .collect::<String>()
        .trim()
        .to_string()
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

    // NB: the model overrides (per-agent config default, then per-turn Smooth
    // Modes) are applied below, AFTER the per-agent `AgentBehaviorConfig` resolves
    // (SEAM 3) — see `apply_agent_model_override` + `apply_model_override`. Nothing
    // between here and there reads `llm.model`.

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

    // Rich Interactions: always wired in the WS server (the primitive is on;
    // the per-agent `enabled_tools` allow-list can restrict individual raise
    // tools). Rich vs conversational-fallback is decided PER KIND from the
    // capabilities the client declared at create-session.
    let interactions = Some(crate::runner::InteractionConfig {
        session_id: session.session_id.clone(),
        kinds: Arc::clone(&state.interactions),
        capabilities: state.session_capabilities(session_id),
        register: {
            let state = state.clone();
            Arc::new(
                move |sid: &str, interaction_id: &str, kind: &str, spec: &Value, responder| {
                    state.register_interaction(
                        sid,
                        crate::state::PendingInteraction {
                            interaction_id: interaction_id.to_string(),
                            kind: kind.to_string(),
                            spec: spec.clone(),
                            responder,
                        },
                    );
                },
            )
        },
        clear: {
            let state = state.clone();
            Arc::new(move |sid: &str| state.clear_interaction(sid))
        },
        attach: {
            let state = state.clone();
            let sid = session.session_id.clone();
            Arc::new(move |kind, values| attach_interaction_effect(&state, &sid, kind, values))
        },
    });

    // The reference server is single-org; a multi-tenant host derives this from    // The reference server is single-org; a multi-tenant host derives this from
    // auth. Used to (a) resolve the org's persona override (SEAM 2) and (b)
    // scope the host's tool provider (SEAM 1).
    let org_id = crate::server::SEED_ORG_ID.to_string();

    // SEAM 3 — per-agent behavior config (instructions + conversation workflow),
    // resolved by the connection's `agent_id` so two agents in the same org can
    // behave differently. Absent / malformed ⇒ `None`, so the org-default persona
    // (SEAM 2) is used, unchanged. Isolated per agent by construction.
    let agent_cfg: Option<AgentBehaviorConfig> =
        state.agent_config.resolve(&session.agent_id).await;

    // SEAM 3 — model precedence, applied low → high so the winner clobbers last:
    //   1. server default (`SMOOTH_AGENT_MODEL`, already in `llm`),
    //   2. the per-AGENT `model` override (when configured),
    //   3. the per-TURN `send_message.model` (Smooth Modes) — always wins.
    let llm = apply_agent_model_override(llm, agent_cfg.as_ref());
    let llm = apply_model_override(llm, parsed);

    // SEAM 3 — per-agent agent-loop cap: the resolved `max_iterations`, else the
    // server default (`SMOOTH_AGENT_MAX_ITERATIONS`). Computed here (not in the
    // spawned turn) so the `Copy` value is simply moved into the task below.
    let max_iterations = agent_cfg
        .as_ref()
        .and_then(|c| c.max_iterations)
        .unwrap_or(state.config.max_iterations);

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

    // SEP per-agent extension enablement (SMOODEV-2259). A resolved agent (Some
    // cfg) always yields `Some(vec)` — even an EMPTY vec — so the extension host
    // intersects the server allowlist with these ids and a resolved agent that
    // enables no extension loads ZERO (fail-closed). `None` only when no per-agent
    // config resolved at all (bare/standalone operator), preserving the
    // server-allowlist-only behavior. Extensions can intercept & mutate tool calls,
    // so a public agent must never silently inherit one.
    let enabled_extensions: Option<Vec<String>> = agent_cfg
        .as_ref()
        .map(AgentBehaviorConfig::enabled_extension_ids);

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
            enabled_extensions.as_deref(),
        )
        .await;
        // Clamp max_tokens to the resolved model's output ceiling (best-effort;
        // None ⇒ unclamped). Reuses the cached /model/info fetch. EPIC th-1cc9fa.
        let model_max_output =
            crate::admin::model_output_ceiling(&state_for_turn, &llm.model).await;
        let result = runner::run_streaming_turn(
            TurnRequest {
                storage: state_for_turn.storage.clone(),
                llm,
                max_iterations,
                conversation_id: &conversation_id,
                request_id: &request_id_owned,
                user_message: &message,
                // The resolved model's output ceiling (clamps max_tokens; None ⇒ unclamped).
                model_max_output,
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
                // Rich Interactions (per-kind card park on capable clients;
                // validated conversational fallback otherwise).
                interactions,
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
                let response =
                    runner::general_agent_response(&turn.reply, &turn.suggested_next_actions);
                let _ = sink_owned.send(protocol::eventual_response(
                    &request_id_owned,
                    200,
                    &turn.message_id,
                    response,
                    false,
                    &turn.citations,
                    turn.usage,
                ));

                // Best-effort auto-title (fires only while the conversation is
                // still default-named ⇒ effectively the first turn). Detached so
                // the small-model call never delays this turn; a failure just
                // leaves the default `Session <uuid>` name.
                let title_state = state_for_turn.clone();
                let title_conv = conversation_id.clone();
                let title_user = message.clone();
                let title_reply = turn.reply.clone();
                tokio::spawn(async move {
                    maybe_auto_title(&title_state, &title_conv, &title_user, &title_reply).await;
                });
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

/// Apply a per-agent `model` override (from the resolved [`AgentBehaviorConfig`])
/// to a config. `Some(model)` sets this agent's default gateway model, overriding
/// the server default; `None` (no per-agent config, or no `model` set) leaves the
/// config unchanged. `from_row_values` already rejects blank models, but a defensive
/// trim keeps this a no-op on whitespace. An explicit per-turn `send_message.model`
/// is layered on top by [`apply_model_override`] and wins.
fn apply_agent_model_override(mut llm: LlmConfig, cfg: Option<&AgentBehaviorConfig>) -> LlmConfig {
    if let Some(model) = cfg.and_then(|c| c.model.as_deref()) {
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

/// The kind-routed host effect of an accepted interaction. For
/// `identity_intake`, stamp the validated values onto the session (metadata
/// `userName` / `contactEmail` / `contactPhone` — the same keys the pre-chat /
/// create path stashes and the OTP contact seam reads). Future kinds add their
/// effect here (or a host overrides the attach seam entirely).
fn attach_interaction_effect(state: &AppState, session_id: &str, kind: &str, values: &Value) {
    if kind == "identity_intake" {
        if let Ok(values) = serde_json::from_value::<IntakeValues>(values.clone()) {
            state.attach_session_identity(session_id, &values);
        }
    }
}

/// `submit_interaction` — resume a turn parked on a Rich Interaction.
///
/// Per `spec/actions/submit-interaction.schema.json` the client sends
/// `{ action, sessionId, requestId, interactionId, kind?, values?, declined? }`
/// in reply to an `interaction_required` event. Validation is **server-side**,
/// routed to the parked kind's validator against the spec the raise carried:
///   - invalid → an `interaction_invalid` event with per-field errors; the
///     turn STAYS parked so the card can resubmit (mirrors `otp_invalid`);
///   - valid → the kind's host effect runs (identity_intake: session identity
///     attach), the parked raise resumes with the canonical values, and an
///     `immediate_response` acks;
///   - `declined: true` → the raise resumes with a declined payload.
///
/// The `interactionId` must echo the event's, so a stale submit can never
/// resolve a newer park; taking the responder only on resolution makes a
/// duplicate submit a no-op (`NO_PENDING_INTERACTION`).
fn handle_submit_interaction(
    state: &AppState,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    // requestId is load-bearing (it echoes the originating
    // interaction_required); require it.
    let Some(request_id) = request_id else {
        let _ = sink.send(protocol::error(
            None,
            "VALIDATION_ERROR",
            "submit_interaction requires a 'requestId'",
        ));
        return;
    };

    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "submit_interaction requires a 'sessionId'",
        ));
        return;
    };

    // Peek the pending interaction WITHOUT consuming the park — an invalid
    // submit must leave the turn parked for a resubmit.
    let Some(pending) = state.pending_interaction(session_id) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "NO_PENDING_INTERACTION",
            &format!("no interaction is awaiting submission for session '{session_id}'"),
        ));
        return;
    };

    // The submit must target THIS interaction instance (and, when it names a
    // kind, the right kind) — a stale card can never resolve a newer park.
    let interaction_id = parsed.get("interactionId").and_then(Value::as_str);
    if interaction_id != Some(pending.interaction_id.as_str()) {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "INTERACTION_MISMATCH",
            "the submitted 'interactionId' does not match the pending interaction",
        ));
        return;
    }
    if let Some(kind) = parsed.get("kind").and_then(Value::as_str) {
        if kind != pending.kind {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "INTERACTION_MISMATCH",
                &format!(
                    "the pending interaction is '{}', not '{kind}'",
                    pending.kind
                ),
            ));
            return;
        }
    }

    // Decline path: resume the raise with a declined payload.
    if parsed
        .get("declined")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if resolve_interaction(
            state,
            session_id,
            request_id,
            InteractionOutcome::Declined,
            sink,
        ) {
            let _ = sink.send(protocol::immediate_response(
                Some(request_id),
                200,
                "Interaction declined",
                json!({ "sessionId": session_id, "interactionId": pending.interaction_id, "declined": true }),
            ));
        }
        return;
    }

    // Values path: route to the parked kind's server-side validator.
    let Some(values) = parsed.get("values") else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "submit_interaction requires 'values' (or 'declined': true)",
        ));
        return;
    };
    let Some(kind) = state.interactions.get(&pending.kind) else {
        // A parked kind the registry no longer hosts (shouldn't happen).
        let _ = sink.send(protocol::error(
            Some(request_id),
            "NO_PENDING_INTERACTION",
            &format!(
                "interaction kind '{}' is not hosted by this server",
                pending.kind
            ),
        ));
        return;
    };

    match kind.validate(&pending.spec, values) {
        Err(errors) => {
            // Retryable: the turn stays parked; the client re-renders the card
            // with the per-field errors (never a terminal `error` event).
            let _ = sink.send(protocol::interaction_invalid(
                request_id,
                &pending.interaction_id,
                &pending.kind,
                &errors,
                "Some fields need attention.",
            ));
        }
        Ok(canonical) => {
            // Run the kind's host effect, then resume the parked raise.
            attach_interaction_effect(state, session_id, &pending.kind, &canonical);
            if resolve_interaction(
                state,
                session_id,
                request_id,
                InteractionOutcome::Submitted {
                    values: canonical.clone(),
                },
                sink,
            ) {
                let _ = sink.send(protocol::immediate_response(
                    Some(request_id),
                    200,
                    "Interaction submitted",
                    json!({
                        "sessionId": session_id,
                        "interactionId": pending.interaction_id,
                        "kind": pending.kind,
                        "values": canonical,
                    }),
                ));
            }
        }
    }
}

/// Take the pending interaction responder for `session_id` and feed it
/// `outcome`. Returns `true` when the parked turn was resumed; emits
/// `NO_PENDING_INTERACTION` and returns `false` when the park raced away
/// (duplicate submit, or the parked turn ended before the submit landed).
fn resolve_interaction(
    state: &AppState,
    session_id: &str,
    request_id: &str,
    outcome: InteractionOutcome,
    sink: &UnboundedSender<Value>,
) -> bool {
    let Some(pending) = state.take_interaction(session_id) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "NO_PENDING_INTERACTION",
            &format!("no interaction is awaiting submission for session '{session_id}'"),
        ));
        return false;
    };
    if pending.responder.send(outcome).is_err() {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "NO_PENDING_INTERACTION",
            &format!(
                "the turn awaiting an interaction for session '{session_id}' is no longer active"
            ),
        ));
        return false;
    }
    true
}

/// `verify_otp` — validate a submitted OTP code and, on success, mark the/// `verify_otp` — validate a submitted OTP code and, on success, mark the
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

    fn cfg_with_model(model: Option<&str>) -> AgentBehaviorConfig {
        AgentBehaviorConfig {
            model: model.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn agent_model_override_present_replaces_default() {
        let cfg = cfg_with_model(Some("claude-sonnet-5"));
        assert_eq!(
            apply_agent_model_override(base_llm(), Some(&cfg)).model,
            "claude-sonnet-5"
        );
    }

    #[test]
    fn agent_model_override_absent_keeps_default() {
        // No per-agent config at all.
        assert_eq!(
            apply_agent_model_override(base_llm(), None).model,
            "claude-haiku-4-5"
        );
        // Config present but no model set.
        let cfg = cfg_with_model(None);
        assert_eq!(
            apply_agent_model_override(base_llm(), Some(&cfg)).model,
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn per_turn_model_wins_over_per_agent() {
        // Precedence as wired in `process_send_message`: agent override first,
        // then the per-turn body override on top — the turn model must win.
        let cfg = cfg_with_model(Some("claude-sonnet-5"));
        let body = json!({ "model": "claude-opus-4-8" });
        let llm = apply_model_override(apply_agent_model_override(base_llm(), Some(&cfg)), &body);
        assert_eq!(llm.model, "claude-opus-4-8");
    }

    #[test]
    fn sanitize_title_strips_quotes_markdown_and_collapses_whitespace() {
        // Wrapping double quotes + trailing newline.
        assert_eq!(
            sanitize_title("\"Reset password help\"\n"),
            "Reset password help"
        );
        // Markdown bold wrapping.
        assert_eq!(sanitize_title("**Billing question**"), "Billing question");
        // Code-fence backticks + collapse internal newlines/spaces.
        assert_eq!(
            sanitize_title("`Order   status\ncheck`"),
            "Order status check"
        );
        // Leading markdown heading marker.
        assert_eq!(sanitize_title("# Refund request"), "Refund request");
        // Inner apostrophe is preserved (only wrapping quotes stripped).
        assert_eq!(sanitize_title("What's my balance"), "What's my balance");
    }

    #[test]
    fn sanitize_title_blank_is_empty() {
        assert_eq!(sanitize_title(""), "");
        assert_eq!(sanitize_title("   \n\t "), "");
        // Only wrapping symbols ⇒ empty (callers reject).
        assert_eq!(sanitize_title("\"\"  "), "");
    }

    #[test]
    fn sanitize_title_caps_length() {
        let long = "word ".repeat(40); // 200 chars
        let out = sanitize_title(&long);
        assert!(out.chars().count() <= TITLE_MAX, "capped: {out:?}");
    }

    #[test]
    fn title_request_body_gives_reasoning_headroom() {
        // Regression: the title model is a reasoning model, so max_tokens must
        // leave room for reasoning + the title. The original 32 was fully eaten
        // by reasoning tokens and yielded empty content. Guard a generous budget.
        let body = title_request_body("What is the capital of France?", "Paris.");
        let max = body["max_tokens"].as_u64().expect("max_tokens present");
        assert!(max >= 256, "auto-title needs reasoning headroom, got {max}");
        assert_eq!(body["model"], AUTO_TITLE_MODEL);
        let prompt = body["messages"][0]["content"].as_str().unwrap();
        assert!(
            prompt.contains("capital of France"),
            "prompt carries the user message"
        );
        assert!(prompt.contains("Paris."), "prompt carries the reply");
    }

    #[test]
    fn per_agent_model_used_when_turn_body_absent() {
        // No per-turn model → the per-agent default stands.
        let cfg = cfg_with_model(Some("claude-sonnet-5"));
        let body = json!({ "message": "hi" });
        let llm = apply_model_override(apply_agent_model_override(base_llm(), Some(&cfg)), &body);
        assert_eq!(llm.model, "claude-sonnet-5");
    }
}
