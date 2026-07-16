//! Wire protocol — server→client event envelopes built to match `spec/`.
//!
//! Every constructor here produces a `serde_json::Value` whose field names and
//! nesting match the JSON Schemas in `smooth-operator/spec/events/*.json` exactly,
//! so the generated TS/Go/.NET/Python clients deserialize them unmodified.
//!
//! All events are serialized as a flat envelope with a `type` discriminator
//! plus the per-event fields documented in `envelope.schema.json`'s
//! `EventEnvelope` (`requestId`, `status`, `data`, `node`, `token`, `error`,
//! `timestamp`).

use serde_json::{json, Value};

/// Current Unix epoch milliseconds (for the `timestamp` field).
#[must_use]
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `pong` — reply to a `ping`. Carries the server timestamp both at the top
/// level and inside `data` (per `pong.schema.json`).
#[must_use]
pub fn pong(request_id: Option<&str>) -> Value {
    let ts = now_ms();
    let mut ev = json!({
        "type": "pong",
        "timestamp": ts,
        "data": { "timestamp": ts },
    });
    set_request_id(&mut ev, request_id);
    ev
}

/// `immediate_response` — synchronous ack. For non-streaming actions this also
/// carries the full response payload in `data`.
#[must_use]
pub fn immediate_response(
    request_id: Option<&str>,
    status: i64,
    message: &str,
    data: Value,
) -> Value {
    let mut ev = json!({
        "type": "immediate_response",
        "status": status,
        "message": message,
        "data": data,
        "timestamp": now_ms(),
    });
    set_request_id(&mut ev, request_id);
    ev
}

/// `stream_token` — a single streamed LLM token. The token is mirrored both at
/// the envelope level (`token`) and inside `data` (per `stream-token.schema.json`).
#[must_use]
pub fn stream_token(request_id: &str, token: &str) -> Value {
    json!({
        "type": "stream_token",
        "requestId": request_id,
        "token": token,
        "data": { "requestId": request_id, "token": token },
        "timestamp": now_ms(),
    })
}

/// `stream_reasoning` — a single streamed *reasoning* token from a reasoning
/// model's separate thinking channel. Shaped exactly like `stream_token`, but
/// on a distinct `type` so clients render it as "thinking" and never fold it
/// into the answer. Clients that don't know the type simply ignore it (the
/// answer still streams via `stream_token`).
#[must_use]
pub fn stream_reasoning(request_id: &str, token: &str) -> Value {
    json!({
        "type": "stream_reasoning",
        "requestId": request_id,
        "token": token,
        "data": { "requestId": request_id, "token": token },
        "timestamp": now_ms(),
    })
}

/// `stream_preamble` — a single streamed token of the fast-model *preamble*: a
/// short "what I'm about to do" sentence generated in parallel with the main turn
/// to cover the reasoning model's time-to-first-token. Shaped exactly like
/// `stream_token`, but on a distinct `type` so clients render it as an *ephemeral*
/// status line that the real answer replaces — never folded into the answer.
/// Clients that don't know the type simply ignore it. Pearl th-9a5794.
#[must_use]
pub fn stream_preamble(request_id: &str, token: &str) -> Value {
    json!({
        "type": "stream_preamble",
        "requestId": request_id,
        "token": token,
        "data": { "requestId": request_id, "token": token },
        "timestamp": now_ms(),
    })
}

/// `stream_chunk` — a per-node state snapshot. `node` is mirrored at the
/// envelope level and inside `data` (per `stream-chunk.schema.json`). `state`
/// only carries safe-to-expose fields.
#[must_use]
pub fn stream_chunk(request_id: &str, node: &str, state: Value) -> Value {
    json!({
        "type": "stream_chunk",
        "requestId": request_id,
        "node": node,
        "data": { "requestId": request_id, "node": node, "state": state },
        "timestamp": now_ms(),
    })
}

/// Per-turn token-accounting + cost, captured from the engine's terminal
/// [`AgentEvent::Completed`](smooth_operator_core::AgentEvent::Completed) and
/// surfaced on the `eventual_response` so clients can accumulate a live session
/// cost. All fields are accumulated across every LLM call in the turn. `Copy` so
/// it threads through the runner → handler → protocol by value.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TurnUsage {
    /// Accumulated cost in USD for this turn (gateway-priced).
    pub cost_usd: f64,
    /// Accumulated prompt (input) tokens for this turn.
    pub prompt_tokens: u64,
    /// Accumulated completion (output) tokens for this turn.
    pub completion_tokens: u64,
}

/// `eventual_response` — the terminal event of a streaming turn. The payload is
/// double-nested (`data.data`) per `eventual-response.schema.json`.
///
/// `citations` are the sources that grounded the answer. They're attached to
/// the inner `data.data.citations` array only when non-empty — absent otherwise,
/// keeping the event back-compatible with clients that predate citations.
///
/// `usage`, when `Some`, attaches the turn's token-accounting + cost as a sibling
/// `data.data.usage` object (`{ costUsd, promptTokens, completionTokens }`) so a
/// client can accumulate live session cost. Absent when the engine reported no
/// usage (e.g. an offline mock turn), keeping the event back-compatible with
/// clients that predate cost reporting.
///
/// `directive`, when `Some`, attaches an opaque client-side directive a host tool
/// emitted this turn as a sibling `data.data.directive` value. The protocol layer
/// never interprets the shape (the host client owns it, like `response`). Absent
/// when the turn produced no directive, keeping the event back-compatible with
/// clients that predate directives.
#[must_use]
pub fn eventual_response(
    request_id: &str,
    status: i64,
    message_id: &str,
    response: Value,
    needs_escalation: bool,
    citations: &[smooth_operator::domain::Citation],
    usage: Option<TurnUsage>,
    directive: Option<Value>,
) -> Value {
    let mut inner = json!({
        "messageId": message_id,
        "response": response,
        "needsEscalation": needs_escalation,
    });
    // Optional + back-compat: only emit `citations` when the turn had sources.
    if !citations.is_empty() {
        inner["citations"] = serde_json::to_value(citations).unwrap_or(Value::Null);
    }
    // Optional + back-compat: only emit `usage` when the engine reported it.
    if let Some(usage) = usage {
        inner["usage"] = json!({
            "costUsd": usage.cost_usd,
            "promptTokens": usage.prompt_tokens,
            "completionTokens": usage.completion_tokens,
        });
    }
    // Optional + back-compat: only emit `directive` when a host tool wrote one.
    if let Some(directive) = directive {
        inner["directive"] = directive;
    }
    json!({
        "type": "eventual_response",
        "requestId": request_id,
        "status": status,
        "data": {
            "requestId": request_id,
            "status": status,
            "data": inner,
        },
        "timestamp": now_ms(),
    })
}

/// `write_confirmation_required` — emitted mid-turn when the agent calls a
/// state-mutating tool that requires explicit human approval before it runs. The
/// turn is **parked** (the agent loop blocks inside the core
/// `ConfirmationHook::pre_call`, corresponding to
/// `AgentEvent::HumanInputRequired { Confirm }`) until the client replies with a
/// `confirm_tool_action` action carrying the same `requestId` and an `approved`
/// boolean.
///
/// Wire shape matches `spec/events/write-confirmation-required.schema.json`
/// exactly (the generated TS/Go/.NET/Python clients deserialize it unmodified):
/// the `requestId` echoes the originating `send_message`, and the prompt detail
/// is double-nested under `data.data.{ toolId, actionDescription }`. `tool_id` is
/// an opaque correlation handle (the runner uses the tool name — a turn parks one
/// tool at a time); `action_description` is the human-readable prompt the client
/// renders in its confirmation dialog.
#[must_use]
pub fn write_confirmation_required(
    request_id: &str,
    tool_id: &str,
    action_description: &str,
) -> Value {
    json!({
        "type": "write_confirmation_required",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": {
                "toolId": tool_id,
                "actionDescription": action_description,
            },
        },
        "timestamp": now_ms(),
    })
}

/// `otp_verification_required` — emitted after a turn's auth gate refused an
/// `end_user` tool on an unverified session and the host has an OTP service
/// installed. Tells the client to collect a one-time code. Wire shape matches
/// `spec/events/otp-verification-required.schema.json` (double-nested
/// `data.data`). `available_channels` are the delivery channels the server can
/// offer given the session's known contacts (`email` / `sms`).
#[must_use]
pub fn otp_verification_required(
    request_id: &str,
    tool_id: &str,
    action_description: &str,
    available_channels: &[&str],
    auth_level: &str,
) -> Value {
    json!({
        "type": "otp_verification_required",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": {
                "toolId": tool_id,
                "actionDescription": action_description,
                "availableChannels": available_channels,
                "authLevel": auth_level,
            },
        },
        "timestamp": now_ms(),
    })
}

/// `interaction_required` — the Rich Interactions envelope: emitted mid-turn
/// when an agent's raise tool parks awaiting the visitor on a session that
/// declared the kind's render capability. Wire shape matches
/// `spec/events/interaction-required.schema.json` (double-nested
/// `data.data.{ interactionId, kind, spec, reason }`). The client renders the
/// kind's card and replies with a `submit_interaction` action carrying the
/// same `requestId` + `interactionId`.
#[must_use]
pub fn interaction_required(
    request_id: &str,
    interaction_id: &str,
    kind: &str,
    spec: &Value,
    reason: &str,
) -> Value {
    json!({
        "type": "interaction_required",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": {
                "interactionId": interaction_id,
                "kind": kind,
                "spec": spec,
                "reason": reason,
            },
        },
        "timestamp": now_ms(),
    })
}

/// `interaction_invalid` — emitted when a `submit_interaction` carried values
/// that failed the kind's server-side validation. The turn REMAINS parked; the
/// client re-renders the card with the per-field `errors`. Mirrors
/// `otp_invalid` (retryable, never a terminal `error`). Wire shape matches
/// `spec/events/interaction-invalid.schema.json`.
#[must_use]
pub fn interaction_invalid(
    request_id: &str,
    interaction_id: &str,
    kind: &str,
    errors: &[smooth_operator::InteractionFieldError],
    message: &str,
) -> Value {
    json!({
        "type": "interaction_invalid",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": {
                "interactionId": interaction_id,
                "kind": kind,
                "errors": errors,
                "message": message,
            },
        },
        "timestamp": now_ms(),
    })
}

/// `otp_sent` — acknowledgement that a code was dispatched to the caller. Wire
/// shape matches `spec/events/otp-sent.schema.json`. `masked_destination` is a
/// partially masked address safe to display (e.g. `j***@example.com`).
#[must_use]
pub fn otp_sent(request_id: &str, channel: &str, masked_destination: &str) -> Value {
    json!({
        "type": "otp_sent",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": {
                "channel": channel,
                "maskedDestination": masked_destination,
            },
        },
        "timestamp": now_ms(),
    })
}

/// `otp_verified` — emitted when a `verify_otp` attempt succeeds. The session is
/// now identity-verified; the client re-sends its message to run the gated tool.
/// Wire shape matches `spec/events/otp-verified.schema.json`.
#[must_use]
pub fn otp_verified(request_id: &str, message: &str) -> Value {
    json!({
        "type": "otp_verified",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": { "message": message },
        },
        "timestamp": now_ms(),
    })
}

/// `otp_invalid` — emitted when a `verify_otp` attempt is rejected. `error` is an
/// optional machine-readable reason (`INVALID_CODE` / `MAX_ATTEMPTS` /
/// `NOT_FOUND` / `EXPIRED`); `attempts_remaining` of 0 means the code is locked
/// and the client must restart the flow. Wire shape matches
/// `spec/events/otp-invalid.schema.json`.
#[must_use]
pub fn otp_invalid(
    request_id: &str,
    error: Option<&str>,
    attempts_remaining: u32,
    message: &str,
) -> Value {
    let mut inner = json!({
        "attemptsRemaining": attempts_remaining,
        "message": message,
    });
    // Optional per spec: only emit `error` when the host determined a cause.
    if let Some(err) = error {
        inner["error"] = json!(err);
    }
    json!({
        "type": "otp_invalid",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": inner,
        },
        "timestamp": now_ms(),
    })
}

/// `error` — an unrecoverable error. The `{ code, message }` descriptor is
/// duplicated at the envelope level and nested under `data.error` for wire
/// backward-compatibility (per `error.schema.json`).
#[must_use]
pub fn error(request_id: Option<&str>, code: &str, message: &str) -> Value {
    let err = json!({ "code": code, "message": message });
    let mut data = json!({ "error": err });
    if let Some(rid) = request_id {
        data["requestId"] = json!(rid);
    }
    let mut ev = json!({
        "type": "error",
        "error": err,
        "data": data,
        "timestamp": now_ms(),
    });
    set_request_id(&mut ev, request_id);
    ev
}

fn set_request_id(ev: &mut Value, request_id: Option<&str>) {
    if let Some(rid) = request_id {
        ev["requestId"] = json!(rid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pong_carries_timestamp_both_places() {
        let ev = pong(Some("r1"));
        assert_eq!(ev["type"], "pong");
        assert_eq!(ev["requestId"], "r1");
        assert!(ev["timestamp"].is_i64());
        assert_eq!(ev["timestamp"], ev["data"]["timestamp"]);
    }

    #[test]
    fn stream_token_mirrors_token() {
        let ev = stream_token("r1", "Hel");
        assert_eq!(ev["type"], "stream_token");
        assert_eq!(ev["token"], "Hel");
        assert_eq!(ev["data"]["token"], "Hel");
        assert_eq!(ev["data"]["requestId"], "r1");
    }

    #[test]
    fn stream_reasoning_is_distinct_type_but_mirrors_token() {
        let ev = stream_reasoning("r1", "let me think");
        // Distinct type so clients never fold it into the answer…
        assert_eq!(ev["type"], "stream_reasoning");
        // …but shaped exactly like stream_token so they render it the same way.
        assert_eq!(ev["token"], "let me think");
        assert_eq!(ev["data"]["token"], "let me think");
        assert_eq!(ev["data"]["requestId"], "r1");
    }

    #[test]
    fn stream_preamble_is_distinct_type_but_mirrors_token() {
        let ev = stream_preamble("r1", "Let me pull up your recent conversations.");
        // Distinct type so clients render it as an ephemeral status line…
        assert_eq!(ev["type"], "stream_preamble");
        // …but shaped exactly like stream_token so they can reuse the render path.
        assert_eq!(ev["token"], "Let me pull up your recent conversations.");
        assert_eq!(
            ev["data"]["token"],
            "Let me pull up your recent conversations."
        );
        assert_eq!(ev["data"]["requestId"], "r1");
    }

    #[test]
    fn stream_chunk_mirrors_node() {
        let ev = stream_chunk("r1", "knowledge_search", json!({ "rawResponse": "x" }));
        assert_eq!(ev["type"], "stream_chunk");
        assert_eq!(ev["node"], "knowledge_search");
        assert_eq!(ev["data"]["node"], "knowledge_search");
        assert_eq!(ev["data"]["state"]["rawResponse"], "x");
    }

    #[test]
    fn eventual_response_double_nests_payload() {
        let ev = eventual_response(
            "r1",
            200,
            "m1",
            json!({"responseParts": ["hi"]}),
            false,
            &[],
            None,
            None,
        );
        assert_eq!(ev["type"], "eventual_response");
        assert_eq!(ev["status"], 200);
        assert_eq!(ev["data"]["data"]["messageId"], "m1");
        assert_eq!(ev["data"]["data"]["needsEscalation"], false);
        assert_eq!(ev["data"]["data"]["response"]["responseParts"][0], "hi");
    }

    #[test]
    fn eventual_response_omits_citations_when_empty() {
        // Back-compat: no `citations` key at all when the turn had no sources.
        let ev = eventual_response(
            "r1",
            200,
            "m1",
            json!({"responseParts": ["hi"]}),
            false,
            &[],
            None,
            None,
        );
        assert!(
            ev["data"]["data"].get("citations").is_none(),
            "citations must be absent when empty for back-compat"
        );
    }

    #[test]
    fn eventual_response_omits_usage_when_none() {
        // Back-compat: no `usage` key at all when the engine reported no cost.
        let ev = eventual_response(
            "r1",
            200,
            "m1",
            json!({"responseParts": ["hi"]}),
            false,
            &[],
            None,
            None,
        );
        assert!(
            ev["data"]["data"].get("usage").is_none(),
            "usage must be absent when None for back-compat"
        );
    }

    #[test]
    fn eventual_response_attaches_usage_when_present() {
        let usage = TurnUsage {
            cost_usd: 0.0123,
            prompt_tokens: 1500,
            completion_tokens: 42,
        };
        let ev = eventual_response(
            "r1",
            200,
            "m1",
            json!({"responseParts": ["hi"]}),
            false,
            &[],
            Some(usage),
            None,
        );
        let u = &ev["data"]["data"]["usage"];
        assert!(
            u.is_object(),
            "usage should be a sibling object under data.data"
        );
        let cost = u["costUsd"].as_f64().expect("costUsd is a number");
        assert!((cost - 0.0123).abs() < 1e-9, "costUsd should round-trip");
        assert_eq!(u["promptTokens"], 1500);
        assert_eq!(u["completionTokens"], 42);
    }

    #[test]
    fn eventual_response_attaches_citations_when_present() {
        let citations = vec![
            smooth_operator::domain::Citation {
                id: "doc-1".into(),
                title: "acme/handbook@main#wildlife/quokka.md".into(),
                url: Some("https://github.com/acme/handbook/blob/main/wildlife/quokka.md".into()),
                snippet: "Quokkas are the friendliest marsupial.".into(),
                score: 0.91,
            },
            smooth_operator::domain::Citation {
                id: "doc-2".into(),
                title: "policies/shipping.md".into(),
                url: None,
                snippet: "Standard shipping takes 5 to 7 business days.".into(),
                score: 0.42,
            },
        ];
        let ev = eventual_response(
            "r1",
            200,
            "m1",
            json!({"responseParts": ["hi"]}),
            false,
            &citations,
            None,
            None,
        );
        let cites = &ev["data"]["data"]["citations"];
        assert!(cites.is_array(), "citations should be an array");
        assert_eq!(cites.as_array().unwrap().len(), 2);
        // GitHub-sourced citation carries id + url + snippet on the wire shape.
        assert_eq!(cites[0]["id"], "doc-1");
        assert_eq!(
            cites[0]["url"],
            "https://github.com/acme/handbook/blob/main/wildlife/quokka.md"
        );
        assert_eq!(
            cites[0]["snippet"],
            "Quokkas are the friendliest marsupial."
        );
        // score is an f32 widened to f64 on the wire, so compare with tolerance.
        let score = cites[0]["score"].as_f64().expect("score is a number");
        assert!(
            (score - 0.91).abs() < 1e-4,
            "score should round-trip ~0.91, got {score}"
        );
        // url is omitted (not null) for a source with no web location.
        assert!(
            cites[1].get("url").is_none(),
            "a urless citation should omit `url`, not emit null"
        );
        assert_eq!(cites[1]["id"], "doc-2");
    }

    #[test]
    fn eventual_response_omits_directive_when_none() {
        // Back-compat: no `directive` key at all when no host tool wrote one.
        let ev = eventual_response(
            "r1",
            200,
            "m1",
            json!({"responseParts": ["hi"]}),
            false,
            &[],
            None,
            None,
        );
        assert!(
            ev["data"]["data"].get("directive").is_none(),
            "directive must be absent when None for back-compat"
        );
    }

    #[test]
    fn eventual_response_attaches_directive_when_present() {
        let directive = json!({"kind": "Navigate", "path": "/crm/contacts/42"});
        let ev = eventual_response(
            "r1",
            200,
            "m1",
            json!({"responseParts": ["hi"]}),
            false,
            &[],
            None,
            Some(directive.clone()),
        );
        assert_eq!(ev["data"]["data"]["directive"], directive);
    }

    #[test]
    fn write_confirmation_required_matches_spec_shape() {
        let ev = write_confirmation_required(
            "r1",
            "delete_record",
            "Tool 'delete_record' requires confirmation. Allow?",
        );
        // Per spec/events/write-confirmation-required.schema.json.
        assert_eq!(ev["type"], "write_confirmation_required");
        assert_eq!(ev["requestId"], "r1");
        assert_eq!(ev["data"]["requestId"], "r1");
        let inner = &ev["data"]["data"];
        assert_eq!(inner["toolId"], "delete_record");
        assert!(inner["actionDescription"]
            .as_str()
            .unwrap()
            .contains("delete_record"));
        assert!(ev["timestamp"].is_i64());
    }

    #[test]
    fn otp_verification_required_matches_spec_shape() {
        let ev = otp_verification_required(
            "r1",
            "pay_invoice",
            "Verify your identity to use pay_invoice.",
            &["email"],
            "end_user",
        );
        assert_eq!(ev["type"], "otp_verification_required");
        assert_eq!(ev["requestId"], "r1");
        assert_eq!(ev["data"]["requestId"], "r1");
        let inner = &ev["data"]["data"];
        assert_eq!(inner["toolId"], "pay_invoice");
        assert_eq!(inner["authLevel"], "end_user");
        assert_eq!(inner["availableChannels"][0], "email");
        assert!(inner["actionDescription"]
            .as_str()
            .unwrap()
            .contains("pay_invoice"));
        assert!(ev["timestamp"].is_i64());
    }

    #[test]
    fn interaction_required_matches_spec_shape() {
        let spec = json!({
            "fields": [
                { "key": "email", "required": true, "label": "Work email" },
                { "key": "phone", "required": false },
            ],
        });
        let ev = interaction_required(
            "r1",
            "int-1",
            "identity_intake",
            &spec,
            "to send you the quote",
        );
        // Per spec/events/interaction-required.schema.json.
        assert_eq!(ev["type"], "interaction_required");
        assert_eq!(ev["requestId"], "r1");
        assert_eq!(ev["data"]["requestId"], "r1");
        let inner = &ev["data"]["data"];
        assert_eq!(inner["interactionId"], "int-1");
        assert_eq!(inner["kind"], "identity_intake");
        assert_eq!(inner["reason"], "to send you the quote");
        assert_eq!(inner["spec"]["fields"][0]["key"], "email");
        assert!(ev["timestamp"].is_i64());
    }

    #[test]
    fn interaction_invalid_matches_spec_shape() {
        let errors = vec![smooth_operator::InteractionFieldError {
            field: "email".into(),
            message: "must be a valid email address".into(),
        }];
        let ev = interaction_invalid(
            "r1",
            "int-1",
            "identity_intake",
            &errors,
            "Some fields need attention.",
        );
        assert_eq!(ev["type"], "interaction_invalid");
        assert_eq!(ev["data"]["requestId"], "r1");
        let inner = &ev["data"]["data"];
        assert_eq!(inner["interactionId"], "int-1");
        assert_eq!(inner["kind"], "identity_intake");
        assert_eq!(inner["message"], "Some fields need attention.");
        assert_eq!(inner["errors"][0]["field"], "email");
        assert!(inner["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("valid email"));
    }

    #[test]
    fn otp_sent_matches_spec_shape() {
        let ev = otp_sent("r1", "email", "j***@example.com");
        assert_eq!(ev["type"], "otp_sent");
        assert_eq!(ev["requestId"], "r1");
        assert_eq!(ev["data"]["data"]["channel"], "email");
        assert_eq!(ev["data"]["data"]["maskedDestination"], "j***@example.com");
    }

    #[test]
    fn otp_verified_matches_spec_shape() {
        let ev = otp_verified("r1", "Identity verified successfully.");
        assert_eq!(ev["type"], "otp_verified");
        assert_eq!(ev["data"]["requestId"], "r1");
        assert_eq!(
            ev["data"]["data"]["message"],
            "Identity verified successfully."
        );
    }

    #[test]
    fn otp_invalid_carries_error_and_attempts() {
        let ev = otp_invalid(
            "r1",
            Some("INVALID_CODE"),
            2,
            "Invalid code. 2 attempt(s) remaining.",
        );
        assert_eq!(ev["type"], "otp_invalid");
        let inner = &ev["data"]["data"];
        assert_eq!(inner["error"], "INVALID_CODE");
        assert_eq!(inner["attemptsRemaining"], 2);
        assert!(inner["message"].as_str().unwrap().contains("remaining"));
    }

    #[test]
    fn otp_invalid_omits_error_when_none() {
        // Optional per spec: no `error` key when the host couldn't determine a cause.
        let ev = otp_invalid("r1", None, 0, "Verification failed.");
        assert!(
            ev["data"]["data"].get("error").is_none(),
            "error must be absent when None"
        );
        assert_eq!(ev["data"]["data"]["attemptsRemaining"], 0);
    }

    #[test]
    fn error_duplicates_descriptor() {
        let ev = error(Some("r1"), "VALIDATION_ERROR", "bad");
        assert_eq!(ev["type"], "error");
        assert_eq!(ev["error"]["code"], "VALIDATION_ERROR");
        assert_eq!(ev["data"]["error"]["message"], "bad");
        assert_eq!(ev["data"]["requestId"], "r1");
    }

    #[test]
    fn immediate_response_carries_data() {
        let ev = immediate_response(Some("r1"), 200, "ok", json!({"sessionId": "s1"}));
        assert_eq!(ev["type"], "immediate_response");
        assert_eq!(ev["status"], 200);
        assert_eq!(ev["data"]["sessionId"], "s1");
    }
}
