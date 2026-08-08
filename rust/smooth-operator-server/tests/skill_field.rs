//! `send_message.skill` — server-side skill resolution (pearl th-b30a6a).
//!
//! Before this seam every client resolved a skill itself and prepended the body
//! to the message text, so the wire carried prose and the skill's markdown got
//! persisted into conversation history (and replayed on every later turn). Now
//! the wire carries the **intent** and the server composes.
//!
//! Drives the real `handler::handle_frame` against in-memory storage and a
//! `MockLlmClient`, so this is the contract the polyglot servers mirror:
//!
//!   1. `skill: "<name>"` resolves and lands in the turn's **system prompt**,
//!      while the **persisted user message stays exactly what the user typed**;
//!   2. an unresolvable skill is a terminal `SKILL_NOT_FOUND` error and the turn
//!      **never runs** (fail-closed — silently answering unskilled is
//!      indistinguishable from answering skilled);
//!   3. no resolver installed ⇒ every skill is unknown (a multi-tenant deploy
//!      never serves host skills by accident);
//!   4. an absent `skill` field is byte-for-byte the old behavior.
//!
//! Fully offline: no gateway key, no network.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{MessageQuery, StorageAdapter};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::conversation::Role;
use smooth_operator_core::llm_provider::MockLlmClient;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler;
use smooth_operator_server::skills::DirSkillResolver;
use smooth_operator_server::state::AppState;

/// Keyless: the injected mock provider serves the turn, so no gateway key is
/// needed (the handler falls back to a placeholder LLM config).
fn keyless_config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "claude-haiku-4-5".into(),
        seed_kb: false,
        max_iterations: 2,
        max_tokens: 128,
        storage: StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: Vec::new(),
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

/// A skill root holding one skill, `greet`, with YAML frontmatter the resolver
/// must strip (it is discovery metadata, not instructions).
fn skill_root() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("greet");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: greet\ndescription: say hello like a pirate\n---\nAlways answer in pirate speak.\n",
    )
    .expect("write SKILL.md");
    tmp
}

const SKILL_BODY: &str = "Always answer in pirate speak.";

struct Harness {
    state: AppState,
    storage: Arc<InMemoryStorageAdapter>,
    mock: MockLlmClient,
}

/// Build the server state with the mock provider and (optionally) a filesystem
/// skill resolver rooted at `skill_dir`.
fn harness(skill_dir: Option<&std::path::Path>) -> Harness {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let mock = MockLlmClient::new();
    let mut state =
        AppState::new(storage.clone(), keyless_config()).with_chat_provider(Arc::new(mock.clone()));
    if let Some(dir) = skill_dir {
        state = state.with_skill_resolver(Arc::new(DirSkillResolver::new(vec![dir.to_path_buf()])));
    }
    Harness {
        state,
        storage,
        mock,
    }
}

/// Drive one frame; events land on the returned receiver. The turn is spawned,
/// so the caller polls for the terminal event.
async fn drive(state: &AppState, frame: &Value) -> UnboundedReceiver<Value> {
    let (tx, rx) = unbounded_channel::<Value>();
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        None,
        &handler::UserScope::Unscoped,
        &frame.to_string(),
        &tx,
    )
    .await;
    rx
}

/// Receive events until one of `type == want` arrives; panic on timeout.
async fn recv_until(rx: &mut UnboundedReceiver<Value>, want: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut seen: Vec<String> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            Ok(Some(ev)) => {
                let ty = ev["type"].as_str().unwrap_or_default().to_string();
                if ty == want {
                    return ev;
                }
                seen.push(ty);
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    panic!("timed out waiting for '{want}'; saw: {seen:?}");
}

/// Create a session and return its id.
async fn create_session(state: &AppState) -> String {
    let mut rx = drive(
        state,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs-1",
            "agentId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    let ev = recv_until(&mut rx, "immediate_response").await;
    ev["data"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

/// The system prompt the mock saw on its most recent call.
fn last_system_prompt(mock: &MockLlmClient) -> String {
    let call = mock.last_call().expect("the turn called the model");
    call.messages
        .iter()
        .find(|m| m.role == Role::System)
        .map(|m| m.content.clone())
        .expect("a system message")
}

/// Every inbound (user) message persisted to `conversation_id`, in order.
async fn persisted_user_messages(
    storage: &InMemoryStorageAdapter,
    conversation_id: &str,
) -> Vec<String> {
    storage
        .list_messages_by_conversation(MessageQuery::new(conversation_id, 100))
        .await
        .expect("read messages")
        .messages
        .into_iter()
        .filter(|m| matches!(m.direction, smooth_operator::domain::Direction::Inbound))
        .filter_map(|m| m.content.text)
        .collect()
}

#[tokio::test]
async fn skill_lands_in_the_system_prompt_and_leaves_the_user_message_alone() {
    let root = skill_root();
    let h = harness(Some(root.path()));
    let session_id = create_session(&h.state).await;

    let mut rx = drive(
        &h.state,
        &json!({
            "action": "send_message",
            "requestId": "turn-1",
            "sessionId": session_id,
            "message": "how are you?",
            "skill": "greet",
        }),
    )
    .await;
    recv_until(&mut rx, "eventual_response").await;

    // The skill body reached the model as a system-prompt section.
    let prompt = last_system_prompt(&h.mock);
    assert!(
        prompt.contains("## Skill: greet"),
        "system prompt must carry the skill header; got: {prompt}"
    );
    assert!(
        prompt.contains(SKILL_BODY),
        "system prompt must carry the skill body; got: {prompt}"
    );
    // Frontmatter is discovery metadata, not instructions.
    assert!(
        !prompt.contains("description: say hello like a pirate"),
        "frontmatter must be stripped; got: {prompt}"
    );

    // ...and the user's message is untouched, in the prompt AND in storage.
    let user_turn = h
        .mock
        .last_call()
        .expect("a call")
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.clone())
        .expect("a user message");
    assert_eq!(user_turn, "how are you?");

    let session = h
        .state
        .get_session(&session_id)
        .expect("session still registered");
    let persisted = persisted_user_messages(h.storage.as_ref(), &session.conversation_id).await;
    assert_eq!(
        persisted,
        vec!["how are you?".to_string()],
        "the skill's prose must NOT be persisted into history"
    );
}

#[tokio::test]
async fn unknown_skill_is_fail_closed_and_runs_no_turn() {
    let root = skill_root();
    let h = harness(Some(root.path()));
    let session_id = create_session(&h.state).await;

    let mut rx = drive(
        &h.state,
        &json!({
            "action": "send_message",
            "requestId": "turn-1",
            "sessionId": session_id,
            "message": "how are you?",
            "skill": "no-such-skill",
        }),
    )
    .await;
    let err = recv_until(&mut rx, "error").await;
    assert_eq!(err["requestId"], "turn-1", "got: {err}");
    assert_eq!(err["error"]["code"], "SKILL_NOT_FOUND", "got: {err}");

    assert_eq!(
        h.mock.call_count(),
        0,
        "no turn may run when the requested skill can't be resolved"
    );
}

#[tokio::test]
async fn a_traversal_skill_name_resolves_nothing() {
    let root = skill_root();
    let h = harness(Some(root.path()));
    let session_id = create_session(&h.state).await;

    let mut rx = drive(
        &h.state,
        &json!({
            "action": "send_message",
            "requestId": "turn-1",
            "sessionId": session_id,
            "message": "hi",
            "skill": "../greet",
        }),
    )
    .await;
    let err = recv_until(&mut rx, "error").await;
    assert_eq!(err["error"]["code"], "SKILL_NOT_FOUND", "got: {err}");
    assert_eq!(h.mock.call_count(), 0);
}

#[tokio::test]
async fn skill_without_a_resolver_is_unknown() {
    let h = harness(None);
    let session_id = create_session(&h.state).await;

    let mut rx = drive(
        &h.state,
        &json!({
            "action": "send_message",
            "requestId": "turn-1",
            "sessionId": session_id,
            "message": "hi",
            "skill": "greet",
        }),
    )
    .await;
    let err = recv_until(&mut rx, "error").await;
    assert_eq!(err["error"]["code"], "SKILL_NOT_FOUND", "got: {err}");
    assert_eq!(h.mock.call_count(), 0);
}

#[tokio::test]
async fn absent_skill_field_is_an_ordinary_turn() {
    let root = skill_root();
    let h = harness(Some(root.path()));
    let session_id = create_session(&h.state).await;

    let mut rx = drive(
        &h.state,
        &json!({
            "action": "send_message",
            "requestId": "turn-1",
            "sessionId": session_id,
            "message": "how are you?",
        }),
    )
    .await;
    recv_until(&mut rx, "eventual_response").await;

    let prompt = last_system_prompt(&h.mock);
    assert!(
        !prompt.contains("## Skill:"),
        "no skill was requested; got: {prompt}"
    );
}

#[tokio::test]
async fn an_empty_skill_string_is_treated_as_absent() {
    let root = skill_root();
    let h = harness(Some(root.path()));
    let session_id = create_session(&h.state).await;

    let mut rx = drive(
        &h.state,
        &json!({
            "action": "send_message",
            "requestId": "turn-1",
            "sessionId": session_id,
            "message": "how are you?",
            "skill": "   ",
        }),
    )
    .await;
    recv_until(&mut rx, "eventual_response").await;
    assert!(!last_system_prompt(&h.mock).contains("## Skill:"));
}
