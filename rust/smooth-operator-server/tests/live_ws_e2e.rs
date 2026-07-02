//! Live LLM WebSocket E2E — gated on `SMOOTH_AGENT_E2E=1` + `SMOOAI_GATEWAY_KEY`.
//!
//! Boots the server in-process with a seeded knowledge base and a real gateway
//! key, then drives a full streaming turn over a real WebSocket client and
//! asserts:
//!   1. knowledge grounding — `send_message("What is SmooAI's return window?…")`
//!      streams ≥1 `stream_token`/`stream_chunk` and the final
//!      `eventual_response` text contains "17".
//!   2. per-session memory — within the SAME session, "My name is Zog." then
//!      "What is my name?" → the reply contains "Zog".
//!
//! ## Gating (safe in CI without creds)
//!
//! No-op unless BOTH are set:
//!   - `SMOOTH_AGENT_E2E=1`
//!   - `SMOOAI_GATEWAY_KEY=<key>` (never printed)
//!
//! ## Run locally (does not print the key)
//!
//! ```sh
//! export SMOOAI_GATEWAY_KEY=$(python3 -c \
//!   "import json;print(json.load(open('$HOME/.local/share/opencode/auth.json'))['smooai']['key'])")
//! export SMOOTH_AGENT_E2E=1
//! cargo test -p smooai-smooth-operator-server --test live_ws_e2e \
//!   -- --nocapture --test-threads=1
//! ```

mod common;

use std::time::Duration;

use serde_json::{json, Value};

use smooth_operator_server::config::ServerConfig;

const GATEWAY_URL: &str = "https://llm.smoo.ai/v1";
const CHEAP_MODEL: &str = "claude-haiku-4-5";
/// Generous overall budget per turn — the live gateway + tool loop can take a
/// while, but should not hang.
const TURN_TIMEOUT: Duration = Duration::from_secs(120);

/// Returns the gateway key, or `None` (with a printed skip notice) when the test
/// should be skipped. NEVER prints the key value.
fn gate(test_name: &str) -> Option<String> {
    if std::env::var("SMOOTH_AGENT_E2E").as_deref() != Ok("1") {
        eprintln!("[skip] {test_name}: SMOOTH_AGENT_E2E != \"1\" — skipping live-gateway WS test");
        return None;
    }
    match std::env::var("SMOOAI_GATEWAY_KEY") {
        Ok(key) if !key.trim().is_empty() => Some(key),
        _ => {
            eprintln!("[skip] {test_name}: SMOOAI_GATEWAY_KEY unset/empty — skipping live-gateway WS test");
            None
        }
    }
}

/// A live config pointed at the gateway with the seeded KB enabled.
fn live_config(key: String) -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: GATEWAY_URL.into(),
        gateway_key: Some(key),
        model: CHEAP_MODEL.into(),
        seed_kb: true,
        max_iterations: 6,
        max_tokens: 512,
        storage: smooth_operator_server::config::StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: Vec::new(),
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

/// Create a session over the WS and return its sessionId.
async fn create_session(client: &mut common::Client) -> String {
    common::send_json(
        client,
        &json!({
            "action": "create_conversation_session",
            "requestId": "e2e-cs",
            "agentId": uuid::Uuid::new_v4().to_string(),
            "userName": "Zog E2E",
        }),
    )
    .await;
    let ev = common::recv_json(client).await;
    assert_eq!(
        ev["type"], "immediate_response",
        "session creation failed: {ev}"
    );
    ev["data"]["sessionId"].as_str().unwrap().to_string()
}

/// Drive one streaming turn: send the message, collect every event up to and
/// including `eventual_response`. Returns (all events, final eventual_response).
async fn run_turn(
    client: &mut common::Client,
    session_id: &str,
    request_id: &str,
    message: &str,
) -> (Vec<Value>, Value) {
    common::send_json(
        client,
        &json!({
            "action": "send_message",
            "requestId": request_id,
            "sessionId": session_id,
            "message": message,
        }),
    )
    .await;

    let mut seen = Vec::new();
    let eventual = common::recv_until(client, "eventual_response", &mut seen, TURN_TIMEOUT).await;
    (seen, eventual)
}

/// Extract the final assistant text from an `eventual_response` event. The
/// runner puts the reply in `data.data.response.responseParts[0]`.
fn final_text(eventual: &Value) -> String {
    let resp = &eventual["data"]["data"]["response"];
    if let Some(parts) = resp["responseParts"].as_array() {
        parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        resp.as_str().unwrap_or_default().to_string()
    }
}

#[tokio::test]
async fn live_ws_knowledge_grounded_and_session_memory() {
    let Some(key) = gate("live_ws_knowledge_grounded_and_session_memory") else {
        return;
    };

    let url = common::boot(live_config(key)).await;
    let mut client = common::connect(&url).await;

    let session_id = create_session(&mut client).await;
    eprintln!("[live-ws] session: {session_id}");

    // ---- Turn 1: knowledge-grounded ("17"-day return window) ----
    let (events, eventual) = run_turn(
        &mut client,
        &session_id,
        "turn-1",
        "What is SmooAI's return window? Search the knowledge base.",
    )
    .await;

    let token_events = events
        .iter()
        .filter(|e| e["type"] == "stream_token")
        .count();
    let chunk_events = events
        .iter()
        .filter(|e| e["type"] == "stream_chunk")
        .count();
    eprintln!(
        "[live-ws] turn 1 streamed: {token_events} stream_token, {chunk_events} stream_chunk events"
    );
    // Print a few streamed tokens so the --nocapture output shows real streaming.
    let sample: String = events
        .iter()
        .filter(|e| e["type"] == "stream_token")
        .filter_map(|e| e["token"].as_str())
        .take(40)
        .collect();
    eprintln!("[live-ws] turn 1 token sample: {sample:?}");

    let reply1 = final_text(&eventual);
    eprintln!("[live-ws] turn 1 final reply: {reply1:?}");

    assert!(
        token_events + chunk_events >= 1,
        "expected at least one streamed stream_token or stream_chunk event in turn 1"
    );
    assert_eq!(eventual["status"], 200, "eventual_response should be 200");
    assert!(
        eventual["data"]["data"]["messageId"].as_str().is_some(),
        "eventual_response must carry a messageId: {eventual}"
    );
    assert!(
        reply1.contains("17"),
        "expected grounded answer to contain the retrieved 17-day fact, got: {reply1:?}"
    );

    // ---- Turn 2 + 3: per-session memory ("Zog") ----
    let (_e2, eventual2) = run_turn(
        &mut client,
        &session_id,
        "turn-2",
        "My name is Zog. Just acknowledge briefly.",
    )
    .await;
    eprintln!("[live-ws] turn 2 reply: {:?}", final_text(&eventual2));

    let (_e3, eventual3) = run_turn(
        &mut client,
        &session_id,
        "turn-3",
        "What is my name? Reply with just the name.",
    )
    .await;
    let reply3 = final_text(&eventual3);
    eprintln!("[live-ws] turn 3 reply (memory check): {reply3:?}");

    assert!(
        reply3.to_ascii_uppercase().contains("ZOG"),
        "expected per-session memory: turn 3 should recall 'Zog', got: {reply3:?}"
    );
}
