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

/// `eventual_response` — the terminal event of a streaming turn. The payload is
/// double-nested (`data.data`) per `eventual-response.schema.json`.
///
/// `citations` are the sources that grounded the answer. They're attached to
/// the inner `data.data.citations` array only when non-empty — absent otherwise,
/// keeping the event back-compatible with clients that predate citations.
#[must_use]
pub fn eventual_response(
    request_id: &str,
    status: i64,
    message_id: &str,
    response: Value,
    needs_escalation: bool,
    citations: &[smooth_operator::domain::Citation],
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
        );
        assert!(
            ev["data"]["data"].get("citations").is_none(),
            "citations must be absent when empty for back-compat"
        );
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
