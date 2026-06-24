//! Scenario parity runner — the **reference** (Rust) implementation.
//!
//! Runs every scenario in `spec/conformance/scenarios/*.json` through the Rust
//! server and asserts the normalized protocol output matches. This is the
//! polyglot parity corpus: the Python reference runner
//! (`python/server/tests/test_scenario_parity.py`) ports the same ~one-file
//! state machine into each language's server suite, and when all five run this
//! corpus green, the servers are at protocol parity.
//!
//! Because the Rust server is the **reference**, it MUST pass the corpus. If a
//! scenario fails here, the corpus (or that scenario) is wrong — not the server.
//!
//! ## Why it's deterministic
//!
//! The turn runs against the engine's [`MockLlmClient`] (the Rust analogue of
//! Python's `MockLlmProvider`), seeded from the scenario's `mockLlmScript`. No
//! gateway, no network, no flakiness — the emitted `stream_token` /
//! `eventual_response` sequence is fixed by the script.
//!
//! ## Mock seeding (the one cross-language wrinkle)
//!
//! Python's `MockLlmProvider` keeps a single FIFO script and *synthesizes* the
//! streaming chunks from it, so `push_text("…")` drives both the non-streaming
//! `chat` and the streamed `chat_stream`. Rust's [`MockLlmClient`] keeps the
//! `chat` queue and the `chat_stream` queue **separate** (`push_text` fills only
//! `chat`; `chat_stream` needs an explicit `push_stream`). The reference server's
//! turn streams (`Agent::run_with_channel` → `chat_stream`), so for each
//! `mockLlmScript` entry we seed BOTH queues — the stream queue with the
//! synthesized `StreamEvent`s (so the engine emits `TokenDelta`s that the server
//! turns into `stream_token`s) and the chat queue with the matching response (a
//! defensive belt-and-braces for any non-streaming code path). The synthesized
//! stream mirrors Python's `_stream_chunks`: text → content deltas; a tool call →
//! a `ToolCallStart` + an arguments delta; then `Done`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
use smooth_operator_core::llm_provider::{tool_call_response, MockLlmClient};
use smooth_operator_core::{StreamEvent, Tool, ToolSchema};

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::server::build_state;

/// A deterministic test tool: it ignores its arguments and returns a fixed
/// `result` string, so a tool-call turn is fully reproducible. The Rust analogue
/// of the Python runner's `FunctionTool(func=lambda: result)`. Installed via the
/// host `ToolProvider` seam (the cross-language `server.tools` corpus directive
/// maps onto whatever each server's tool-injection mechanism is).
struct CorpusTool {
    name: String,
    description: String,
    parameters: Value,
    result: String,
}

#[async_trait]
impl Tool for CorpusTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(&self, _arguments: Value) -> anyhow::Result<String> {
        Ok(self.result.clone())
    }
}

/// Contributes a scenario's `server.tools` as deterministic tools on every turn.
struct CorpusToolProvider {
    tools: Vec<Arc<dyn Tool>>,
}

#[async_trait]
impl ToolProvider for CorpusToolProvider {
    async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

/// Build a `ToolProvider` from a scenario's `server.tools` directive, or `None`
/// when the scenario installs no tools.
fn build_tool_provider(scenario: &Value) -> Option<Arc<dyn ToolProvider>> {
    let specs = scenario.get("server")?.get("tools")?.as_array()?;
    if specs.is_empty() {
        return None;
    }
    let tools: Vec<Arc<dyn Tool>> = specs
        .iter()
        .map(|spec| {
            Arc::new(CorpusTool {
                name: spec["name"].as_str().unwrap_or_default().to_string(),
                description: spec
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                parameters: spec
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                result: spec["result"].as_str().unwrap_or_default().to_string(),
            }) as Arc<dyn Tool>
        })
        .collect();
    Some(Arc::new(CorpusToolProvider { tools }))
}

/// Resolve the conformance scenarios directory relative to THIS crate, so the
/// test works regardless of the cwd the harness runs it from. From
/// `rust/smooth-operator-server` the corpus is at `../../spec/conformance/scenarios`.
fn scenarios_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/conformance/scenarios")
        .canonicalize()
        .expect("conformance scenarios directory should exist")
}

/// Every `*.json` scenario, sorted by path for a stable order.
fn scenario_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(scenarios_dir())
        .expect("read scenarios dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    paths
}

/// Split `s` into up to `n` roughly-equal non-empty pieces (mirrors the Python
/// reference's `_split_into_chunks`, so a text reply streams as a few deltas).
fn split_into_chunks(s: &str, n: usize) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = s.chars().collect();
    let parts = n.min(chars.len()).max(1);
    let size = chars.len().div_ceil(parts);
    chars.chunks(size).map(|c| c.iter().collect()).collect()
}

/// Build a `MockLlmClient` from a scenario's `mockLlmScript`, seeding BOTH the
/// stream queue (what the server's streaming turn consumes) and the chat queue.
///
/// Each entry is one assistant turn:
/// - `{ "kind": "text", "text": "…" }` → content `Delta`s + `Done{stop}`, plus a matching `push_text` on the chat queue.
/// - `{ "kind": "toolCall", "name": "…", "arguments": "{…}" }` → a `ToolCallStart` + an arguments `Delta` + `Done{tool_calls}`, plus a matching tool-call chat response.
fn build_mock(script: &[Value]) -> MockLlmClient {
    let mock = MockLlmClient::new();
    for entry in script {
        let kind = entry
            .get("kind")
            .and_then(Value::as_str)
            .expect("mockLlmScript entry needs a 'kind'");
        match kind {
            "text" => {
                let text = entry
                    .get("text")
                    .and_then(Value::as_str)
                    .expect("text entry needs 'text'");
                let mut events: Vec<StreamEvent> = split_into_chunks(text, 3)
                    .into_iter()
                    .map(|content| StreamEvent::Delta { content })
                    .collect();
                events.push(StreamEvent::Done {
                    finish_reason: "stop".into(),
                });
                mock.push_stream(events);
                mock.push_text(text);
            }
            "toolCall" => {
                let id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call-1")
                    .to_string();
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .expect("toolCall entry needs 'name'")
                    .to_string();
                // `arguments` is a JSON string in the corpus (OpenAI shape).
                let arguments_str = entry
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string();
                let arguments: Value =
                    serde_json::from_str(&arguments_str).unwrap_or_else(|_| json!({}));
                mock.push_stream(vec![
                    StreamEvent::ToolCallStart {
                        index: 0,
                        id: id.clone(),
                        name: name.clone(),
                    },
                    StreamEvent::ToolCallArgumentsDelta {
                        index: 0,
                        arguments_chunk: arguments_str,
                    },
                    StreamEvent::Done {
                        finish_reason: "tool_calls".into(),
                    },
                ]);
                // Defensive chat-queue twin (the streaming path is what runs).
                mock.push_response(tool_call_response(id, name, arguments));
            }
            other => panic!("unknown mockLlmScript kind: {other:?}"),
        }
    }
    mock
}

/// A keyless local config for the parity turn — in-memory storage, NO seeded KB
/// (so the deterministic mock reply is the only thing that grounds the
/// `eventual_response`; the corpus doesn't exercise knowledge), no gateway key
/// (the injected mock replaces the live client). `widget_auth_strict` off so a
/// fresh agent id can open a session.
fn parity_config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "mock".into(),
        seed_kb: false,
        max_iterations: 4,
        max_tokens: 128,
        storage: StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: Vec::new(),
    }
}

/// Resolve a dotted path (`data.data.response.responseParts`) into a nested
/// `Value`. Object keys descend; an all-digit segment indexes into an array.
/// Mirrors the Python reference's `_dot`.
fn dot<'a>(value: &'a Value, path: &str) -> &'a Value {
    let mut cur = value;
    for part in path.split('.') {
        cur = if let Ok(idx) = part.parse::<usize>() {
            cur.get(idx)
                .unwrap_or_else(|| panic!("index {idx} out of range in path '{path}': {cur}"))
        } else {
            cur.get(part)
                .unwrap_or_else(|| panic!("missing key '{part}' in path '{path}': {cur}"))
        };
    }
    cur
}

/// Substitute `{{name}}` placeholders in string fields from captured vars
/// (recursing into objects). Mirrors the Python reference's `_subst`.
fn subst(value: &Value, vars: &std::collections::HashMap<String, Value>) -> Value {
    match value {
        Value::String(s) if s.starts_with("{{") && s.ends_with("}}") => {
            let key = &s[2..s.len() - 2];
            vars.get(key)
                .cloned()
                .unwrap_or_else(|| panic!("no captured var '{key}' for substitution"))
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), subst(v, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The next protocol event, skipping non-semantic `keepalive` / `pong` frames.
/// Mirrors the Python reference's `_next_event`.
async fn next_event(client: &mut common::Client) -> Value {
    loop {
        let ev = common::recv_json(client).await;
        match ev.get("type").and_then(Value::as_str) {
            Some("keepalive") | Some("pong") => continue,
            _ => return ev,
        }
    }
}

/// Match the outbound event stream against an ordered list of matchers (one
/// `expect` array). Faithful port of the Python reference's `_match_expected`,
/// including the one-event lookahead a `repeat` matcher uses when its run
/// overruns into the next matcher.
async fn match_expected(
    client: &mut common::Client,
    matchers: &[Value],
    vars: &mut std::collections::HashMap<String, Value>,
) {
    let mut pending: Option<Value> = None;
    for m in matchers {
        let m_type = m
            .get("type")
            .and_then(Value::as_str)
            .expect("matcher 'type'");
        let is_repeat = m.get("repeat").and_then(Value::as_bool).unwrap_or(false);
        let mut accumulated = String::new();
        loop {
            let event = match pending.take() {
                Some(ev) => ev,
                None => next_event(client).await,
            };
            let ev_type = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();

            if is_repeat && ev_type != m_type {
                // The repeated run ended; this event belongs to the next matcher.
                pending = Some(event);
                break;
            }
            assert_eq!(ev_type, m_type, "expected {m_type}, got {ev_type}: {event}");

            if let Some(status) = m.get("status") {
                assert_eq!(
                    event.get("status"),
                    Some(status),
                    "{m_type}: status mismatch (got {:?}, want {status})",
                    event.get("status")
                );
            }
            if let Some(gte) = m.get("statusGte").and_then(Value::as_i64) {
                let got = event
                    .get("status")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MIN);
                assert!(got >= gte, "{m_type}: status {got} < {gte}");
            }
            if let Some(asserts) = m.get("assert").and_then(Value::as_object) {
                for (path, expected) in asserts {
                    let actual = dot(&event, path);
                    assert_eq!(
                        actual, expected,
                        "{m_type}: {path} = {actual} != {expected}"
                    );
                }
            }
            if let Some(captures) = m.get("capture").and_then(Value::as_object) {
                for (var, path) in captures {
                    let path = path.as_str().expect("capture path is a string");
                    vars.insert(var.clone(), dot(&event, path).clone());
                }
            }
            if let Some(field) = m.get("accumulate").and_then(Value::as_str) {
                accumulated.push_str(event.get(field).and_then(Value::as_str).unwrap_or_default());
            }
            if !is_repeat {
                break;
            }
        }
        if let Some(expected) = m.get("assertAccumulated").and_then(Value::as_str) {
            assert_eq!(
                accumulated, expected,
                "{m_type}: accumulated {accumulated:?} != {expected:?}"
            );
        }
    }
}

/// Drive one scenario file end-to-end through the reference server.
async fn run_scenario(path: &Path) {
    let scenario: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read scenario"))
            .unwrap_or_else(|e| panic!("parse scenario {}: {e}", path.display()));

    let script = scenario
        .get("mockLlmScript")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mock = build_mock(&script);

    // Boot the reference server with the injected mock — the exact analogue of
    // the Python reference's `ServerState(chat_client=mock)`. A scenario's
    // `server.tools` directive installs deterministic tools via the host
    // `ToolProvider` seam so tool-calling turns run offline.
    let mut state = build_state(parity_config()).with_chat_provider(Arc::new(mock));
    if let Some(provider) = build_tool_provider(&scenario) {
        state = state.with_tools(provider);
    }
    let url = common::boot_state(state).await;
    let mut client = common::connect(&url).await;

    let mut vars: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let steps = scenario
        .get("steps")
        .and_then(Value::as_array)
        .expect("scenario needs 'steps'");
    for step in steps {
        let send = subst(step.get("send").expect("step needs 'send'"), &vars);
        common::send_json(&mut client, &send).await;
        let expect = step
            .get("expect")
            .and_then(Value::as_array)
            .expect("step needs 'expect'");
        match_expected(&mut client, expect, &mut vars).await;
    }
}

/// The corpus is small and fixed; drive every scenario from one test so a new
/// `*.json` is picked up automatically (mirrors the Python reference's
/// parametrization). Each scenario boots its own server with its own mock.
#[tokio::test]
async fn scenario_parity_corpus() {
    let paths = scenario_paths();
    assert!(
        !paths.is_empty(),
        "no conformance scenarios found in {}",
        scenarios_dir().display()
    );
    for path in &paths {
        eprintln!(
            "[scenario-parity] {}",
            path.file_name().unwrap().to_string_lossy()
        );
        run_scenario(path).await;
    }
    eprintln!("[scenario-parity] {} scenario(s) passed", paths.len());
}
