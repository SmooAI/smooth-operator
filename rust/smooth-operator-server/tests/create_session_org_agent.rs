//! `create_conversation_session` derives the org + agent from the request.
//!
//! Drives the real `handler::handle_frame` create-session path and reads back
//! the persisted conversation + participants to prove the multi-tenant FK
//! columns (`organization_id`, agent `internal_id`) carry the *authenticated*
//! org and the *requested* agent — not the single-org seed stub.
//!
//! Three cases, matching the derivation priority in `handle_create_session`:
//!   1. **Authed principal** (`auth_org = Some("org-X")`) + payload `agentId` →
//!      conversation + participants carry org `X` and the agent carries `Y`.
//!   2. **Widget policy org** wins over the connection's auth org (a widget
//!      visitor's org rides on the agent's embed policy, not a JWT).
//!   3. **Seed fallback** — no auth org, no widget-policy org → the conversation
//!      belongs to the seed org, so the existing single-org/dev behavior is
//!      unchanged.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::domain::ParticipantType;
use smooth_operator::widget_auth::{AgentWidgetAuth, StaticWidgetAuth};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler;
use smooth_operator_server::server::SEED_ORG_ID;
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

/// Drive one `create_conversation_session` frame and return the conversation id
/// from the `immediate_response` once it lands (the handler persists in a
/// spawned task, so we poll the sink, then read storage).
async fn create_session(
    state: &AppState,
    auth_org: Option<&str>,
    origin: Option<&str>,
    frame: &Value,
) -> String {
    let (tx, mut rx) = unbounded_channel::<Value>();
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-test",
        origin,
        auth_org,
        &handler::UserScope::Unscoped,
        &frame.to_string(),
        &tx,
    )
    .await;
    recv_conversation_id(&mut rx).await
}

/// Await the `immediate_response` and pull `conversationId` out of it.
async fn recv_conversation_id(rx: &mut UnboundedReceiver<Value>) -> String {
    let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("create-session should emit an event")
        .expect("sink open");
    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(ev["status"], 200, "got: {ev}");
    ev["data"]["conversationId"]
        .as_str()
        .expect("conversationId")
        .to_string()
}

#[tokio::test]
async fn authed_principal_org_and_payload_agent_carry_through() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let frame = json!({
        "action": "create_conversation_session",
        "requestId": "cs-1",
        "agentId": "agent-Y",
        "userName": "Authed User",
    });
    let conversation_id = create_session(&state, Some("org-X"), None, &frame).await;

    // The conversation is owned by the authenticated org, NOT the seed.
    let conv = storage
        .get_conversation(&conversation_id)
        .await
        .expect("get conversation")
        .expect("conversation persisted");
    assert_eq!(conv.organization_id, "org-X");
    assert_ne!(conv.organization_id, SEED_ORG_ID);

    let participants = storage
        .list_participants_by_conversation(&conversation_id)
        .await
        .expect("list participants");
    assert_eq!(participants.len(), 2, "user + agent participants");
    for p in &participants {
        assert_eq!(p.organization_id, "org-X", "participant org");
    }

    // The agent participant carries the requested agent id (its `internal_id`).
    let agent = participants
        .iter()
        .find(|p| p.participant_type == ParticipantType::AiAgent)
        .expect("agent participant");
    assert_eq!(agent.internal_id.as_deref(), Some("agent-Y"));

    // The session binds to the same agent.
    let sessions = storage
        .list_sessions_by_conversation(&conversation_id)
        .await
        .expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent_id, "agent-Y");
}

#[tokio::test]
async fn widget_policy_org_wins_over_connection_auth_org() {
    let storage = Arc::new(InMemoryStorageAdapter::new());

    // A widget policy that knows the agent's org and allows any origin.
    let mut rows = HashMap::new();
    rows.insert(
        "agent-W".to_string(),
        AgentWidgetAuth {
            allowed_origins: vec!["*".to_string()],
            public_key: None,
            organization_id: Some("org-from-widget".to_string()),
        },
    );
    let state = AppState::new(storage.clone(), base_config())
        .with_widget_auth(Arc::new(StaticWidgetAuth::new(rows)));

    let frame = json!({
        "action": "create_conversation_session",
        "requestId": "cs-2",
        "agentId": "agent-W",
    });
    // Even with a connection auth org, the widget policy's org takes precedence.
    let conversation_id = create_session(
        &state,
        Some("org-from-jwt"),
        Some("https://embed.example"),
        &frame,
    )
    .await;

    let conv = storage
        .get_conversation(&conversation_id)
        .await
        .expect("get conversation")
        .expect("conversation persisted");
    assert_eq!(conv.organization_id, "org-from-widget");
}

#[tokio::test]
async fn no_auth_no_widget_org_falls_back_to_seed() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), base_config());

    let frame = json!({
        "action": "create_conversation_session",
        "requestId": "cs-3",
        "agentId": "agent-Z",
    });
    // No auth org, no widget policy → behavior-preserving seed-org fallback.
    let conversation_id = create_session(&state, None, None, &frame).await;

    let conv = storage
        .get_conversation(&conversation_id)
        .await
        .expect("get conversation")
        .expect("conversation persisted");
    assert_eq!(conv.organization_id, SEED_ORG_ID);
}

/// The turn's org must reach the host **tool provider** (SEAM 1), not just the
/// conversation row.
///
/// `handle_send_message` derived the real org from the conversation (for the
/// gateway key) and then RE-BOUND `org_id` to the seed org a few lines later,
/// shadowing it. Everything downstream of that second binding — the persona
/// override and the host tool provider's scope — saw `reference-org` for every
/// tenant, while the gateway key from the same function stayed correctly
/// per-org. Billing therefore looked right, which is why it went unnoticed
/// until `reference-org` showed up on a customer's `gen_ai.chat` span.
///
/// This asserts the seam, not the span: a host whose tools are org-scoped (the
/// SmooAI chat-ws case) would otherwise scope every tenant's tools to the seed
/// org.
#[tokio::test]
async fn send_message_scopes_the_tool_provider_to_the_conversation_org() {
    use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
    use smooth_operator_core::Tool;
    use std::sync::Mutex;

    /// Captures the org the server scoped this turn's tools to.
    struct OrgRecordingTools(Arc<Mutex<Option<Option<String>>>>);

    #[async_trait::async_trait]
    impl ToolProvider for OrgRecordingTools {
        async fn tools_for(&self, ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
            *self.0.lock().unwrap() = Some(ctx.org_id.clone());
            Vec::new()
        }
    }

    let seen = Arc::new(Mutex::new(None));
    let storage = Arc::new(InMemoryStorageAdapter::new());

    // A live turn needs a resolvable gateway key and an LLM, or the handler
    // returns before it ever consults the tool provider.
    let mock = smooth_operator_core::llm_provider::MockLlmClient::new();
    mock.push_stream(vec![
        smooth_operator_core::llm::StreamEvent::Delta {
            content: "ok".into(),
        },
        smooth_operator_core::llm::StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    let config = ServerConfig {
        gateway_key: Some("test-key".into()),
        ..base_config()
    };
    let state = AppState::new(storage.clone(), config)
        .with_tools(Arc::new(OrgRecordingTools(seen.clone())))
        .with_chat_provider(Arc::new(mock));

    let frame = json!({
        "action": "create_conversation_session",
        "requestId": "cs-tools",
        "agentId": "agent-Y",
        "userName": "Tenant User",
    });
    let conversation_id = create_session(&state, Some("org-TENANT"), None, &frame).await;

    // Sanity: the create path already scoped correctly — so a failure below is
    // squarely the send path, not session creation.
    let conv = storage
        .get_conversation(&conversation_id)
        .await
        .expect("get conversation")
        .expect("conversation persisted");
    assert_eq!(conv.organization_id, "org-TENANT");

    let session_id = storage
        .list_sessions_by_conversation(&conversation_id)
        .await
        .expect("list sessions")
        .first()
        .map(|s| s.session_id.clone())
        .expect("session persisted");

    let (tx, _rx) = unbounded_channel::<Value>();
    let send = json!({
        "action": "send_message",
        "requestId": "sm-1",
        "sessionId": session_id,
        "message": "hello",
    });
    handler::handle_frame(
        &state,
        &AccessContext::anonymous(),
        "conn-test",
        None,
        Some("org-TENANT"),
        &handler::UserScope::Unscoped,
        &send.to_string(),
        &tx,
    )
    .await;

    // The turn runs in a spawned task; poll until the provider is consulted.
    let mut org = None;
    for _ in 0..100 {
        if let Some(v) = seen.lock().unwrap().clone() {
            org = Some(v);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let org = org.expect("tool provider should be consulted for the turn");
    assert_eq!(
        org.as_deref(),
        Some("org-TENANT"),
        "host tools were scoped to {org:?} instead of the conversation's org — \
         the seed-org re-binding in handle_send_message is back"
    );
    assert_ne!(org.as_deref(), Some(SEED_ORG_ID));
}
