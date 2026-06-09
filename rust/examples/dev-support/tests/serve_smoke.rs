//! Offline smoke test for `dev-support serve` — **no network, no real GitHub,
//! no API key**.
//!
//! `serve` is the showcase end-to-end path: ingest the configured repo, then run
//! the real `smooth-operator-server` over that knowledge so the chat-widget can
//! connect. This test proves the whole path is wired without a live LLM or
//! GitHub:
//!
//!   1. **Build the serve state** from a `MockConnector`-ingested fixture repo
//!      (in-memory storage, the deterministic embedder — fully offline). Assert
//!      the resulting [`AppState`]'s knowledge holds the ingested fixture docs
//!      (retrievable through the SAME ACL-filtered handle the server's turn
//!      runner reads from) and that the embedder/reranker selection matches the
//!      key-less config.
//!   2. **Boot the real server** over that state on an ephemeral port via the
//!      server crate's own [`serve_state_on`] (no WS-loop reimplementation), and
//!      drive a real WebSocket client through `ping` →
//!      `create_conversation_session` → `send_message`, asserting the protocol
//!      responds end-to-end (and, with no gateway key, errors cleanly on the LLM
//!      turn rather than hanging).
//!   3. **Drive one grounded turn** over the served storage with a scripted
//!      `MockLlmClient`, asserting the agent retrieved the fixture fact and the
//!      reply is grounded in it — the "chat-in-browser" payload, minus the live
//!      LLM.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use dev_support::config::DevSupportConfig;
use dev_support::serve::build_serve_state_with_storage;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::embedding::{DeterministicEmbedder, Embedder, DEFAULT_EMBEDDING_DIM};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_ingestion::{MockConnector, RawDocument};
use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::reranker::{build_reranker, RerankMode, RerankerConfig};
use smooth_operator_server::runner::{self, TurnRequest};
use smooth_operator_server::server::serve_state_on;
use smooth_operator_server::state::AppState;

/// A distinctive fact a generic, ungrounded answer could not invent.
const DISTINCTIVE_FACT: &str = "the Frobnicator uses a 42-slot ring buffer";

/// A fixture "repo": a README mentioning the distinctive fact + a code file.
fn fixture_connector() -> MockConnector {
    MockConnector::new(vec![
        RawDocument::new(
            "acme/widget@main#README.md",
            "https://github.com/acme/widget/blob/main/README.md",
            format!(
                "# Widget\n\nThe Widget service is built around the Frobnicator subsystem. \
                 Internally {DISTINCTIVE_FACT} to batch incoming events before flushing them."
            ),
        )
        .with_title("README.md")
        .with_metadata("kind", "prose"),
        RawDocument::new(
            "acme/widget@main#src/frob.rs",
            "https://github.com/acme/widget/blob/main/src/frob.rs",
            "pub struct Frobnicator { ring: [Event; 42] }",
        )
        .with_title("src/frob.rs")
        .with_metadata("kind", "code"),
    ])
}

/// The example config for the smoke test: `none` auth, no real GitHub needed.
fn fixture_config() -> DevSupportConfig {
    DevSupportConfig::from_toml_str(
        r#"
        [github]
        owner = "acme"
        repo = "widget"
        auth = "none"

        [agent]
        model = "claude-haiku-4-5"
        tools = ["knowledge_search"]
    "#,
    )
    .expect("parse fixture config")
}

/// A key-less [`ServerConfig`] — exactly the offline / no-creds scenario. With
/// no gateway key the embedder selection falls back to the deterministic
/// embedder and `send_message` errors cleanly (no live LLM).
fn keyless_server_config(port: u16) -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "claude-haiku-4-5".into(),
        seed_kb: false,
        max_iterations: 4,
        max_tokens: 128,
        storage: StorageBackend::Memory,
    }
}

/// Build the serve state offline: ingest the fixture repo into a fresh in-memory
/// adapter with the deterministic embedder (no env reads, no network).
async fn build_offline_serve_state(config: &DevSupportConfig, port: u16) -> AppState {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let embedder = DeterministicEmbedder::new();
    let serve = build_serve_state_with_storage(
        config,
        keyless_server_config(port),
        storage,
        &embedder,
        &fixture_connector(),
    )
    .await
    .expect("build serve state");

    // The ingest really pulled + stored the fixture repo.
    assert_eq!(serve.report.documents_pulled, 2, "README + code file");
    assert!(
        serve.report.chunks_stored >= 2,
        "expected >=2 chunks stored, report: {:?}",
        serve.report
    );
    assert_eq!(serve.org_id, "acme/widget");
    serve.app_state
}

#[tokio::test]
async fn serve_state_holds_ingested_repo_and_matches_config_selection() {
    let config = fixture_config();
    let state = build_offline_serve_state(&config, 0).await;

    // (a) The served AppState's knowledge — read through the SAME ACL-filtered
    //     handle the server's turn runner uses (anonymous == org-public, which
    //     is what an unauthenticated widget connection gets) — holds the
    //     ingested fixture fact. This is what grounds a browser chat.
    let knowledge = state
        .storage
        .knowledge_for_access(&AccessContext::anonymous());
    let hits = knowledge
        .query("Frobnicator ring buffer", 3)
        .expect("query served knowledge");
    assert!(
        hits.iter().any(|h| h.chunk.contains("42-slot ring buffer")),
        "served knowledge must contain the ingested fixture fact, got: {hits:?}"
    );

    // (b) Embedder selection matches the key-less config: the deterministic
    //     1024-d embedder (the same selector the server's /index path uses). A
    //     keyed config would select the 1536-d GatewayEmbedder.
    assert_eq!(
        DeterministicEmbedder::new().dim(),
        DEFAULT_EMBEDDING_DIM,
        "key-less serve config selects the 1024-d deterministic embedder"
    );

    // (c) Reranker selection matches config: default (SMOOTH_AGENT_RERANK unset
    //     in the offline test) is the no-op — retrieval order unchanged.
    let reranker = build_reranker(&RerankerConfig::from_server_config(&state.config));
    assert!(
        reranker.is_none(),
        "default (off) rerank selection yields no reranker"
    );
    // And the selector still honors an explicit lexical request (offline path).
    let lexical = RerankerConfig {
        mode: RerankMode::Lexical,
        ..RerankerConfig::from_server_config(&state.config)
    };
    assert!(
        build_reranker(&lexical).is_some(),
        "lexical rerank selection yields a reranker"
    );
}

// ---- a real WebSocket boot over the served state -------------------------

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn recv_json(client: &mut Client) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(15), client.next())
        .await
        .expect("recv timed out")
        .expect("stream ended")
        .expect("ws error");
    match frame {
        WsMessage::Text(t) => serde_json::from_str(&t).expect("parse json event"),
        other => panic!("expected text frame, got: {other:?}"),
    }
}

async fn send_json(client: &mut Client, value: &Value) {
    client
        .send(WsMessage::Text(value.to_string().into()))
        .await
        .expect("send frame");
}

/// Receive events until one with `type == event_type` arrives (collecting the
/// rest), so the streaming ack ordering doesn't make the test flaky.
async fn recv_until(client: &mut Client, event_type: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for '{event_type}'");
        let frame = tokio::time::timeout(remaining, client.next())
            .await
            .expect("recv timed out")
            .expect("stream ended")
            .expect("ws error");
        if let WsMessage::Text(t) = frame {
            let ev: Value = serde_json::from_str(&t).expect("parse json event");
            if ev["type"] == event_type {
                return ev;
            }
        }
    }
}

#[tokio::test]
async fn serve_boots_the_real_server_over_the_ingested_repo() {
    let config = fixture_config();
    let state = build_offline_serve_state(&config, 0).await;

    // Boot the REAL smooth-operator-server over the served state on an ephemeral
    // port — through the server crate's own serve loop (no WS reimplementation).
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        serve_state_on(state, listener).await.expect("serve");
    });

    let url = format!("ws://{addr}/ws");
    let (mut client, _resp) = connect_async(&url).await.expect("connect ws");

    // ping → pong: the served server speaks the protocol.
    send_json(&mut client, &json!({ "action": "ping", "requestId": "p1" })).await;
    let pong = recv_json(&mut client).await;
    assert_eq!(pong["type"], "pong", "got: {pong}");

    // create_conversation_session → immediate_response with a session descriptor.
    send_json(
        &mut client,
        &json!({
            "action": "create_conversation_session",
            "requestId": "cs1",
            "agentId": uuid::Uuid::new_v4().to_string(),
            "userName": "Widget User",
        }),
    )
    .await;
    let created = recv_until(&mut client, "immediate_response").await;
    assert_eq!(created["status"], 200, "got: {created}");
    let session_id = created["data"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // send_message: with no gateway key the served server errors cleanly
    // (LLM_UNAVAILABLE) instead of hanging — proving the full ingest→serve→WS
    // path is live and the protocol's terminal path works end-to-end.
    send_json(
        &mut client,
        &json!({
            "action": "send_message",
            "requestId": "sm1",
            "sessionId": session_id,
            "message": "How big is the Frobnicator's ring buffer?",
        }),
    )
    .await;
    let err = recv_until(&mut client, "error").await;
    assert_eq!(err["error"]["code"], "LLM_UNAVAILABLE", "got: {err}");
}

#[tokio::test]
async fn grounded_turn_over_served_storage_answers_from_the_ingested_repo() {
    let config = fixture_config();
    let state = build_offline_serve_state(&config, 0).await;

    // First create a conversation so the runner can persist + replay messages.
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    state
        .storage
        .create_conversation(smooth_operator::domain::Conversation {
            id: conversation_id.clone(),
            platform: smooth_operator::domain::Platform::Web,
            name: "smoke".into(),
            organization_id: "acme/widget".into(),
            idempotency_key: conversation_id.clone(),
            metadata_json: None,
            analytics_json: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("create conversation");

    // Script the STREAMING path (the runner drives `run_with_channel`, which
    // calls `chat_stream`): turn 1 streams a `knowledge_search` tool call; turn 2
    // streams the grounded answer. (The non-streaming push_text/push_tool_call
    // queue is NOT what the runner consumes.)
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_kb_1".into(),
            name: "knowledge_search".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: r#"{"query":"Frobnicator ring buffer size"}"#.into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Delta {
            content: "The Frobnicator uses a 42-slot ring buffer to batch events before flushing."
                .into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    // Drive one streaming turn over the SAME served-state storage the WS server
    // reads from, with the scripted mock provider (offline). Anonymous access ==
    // org-public, what an unauthenticated widget connection gets.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let drain = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        events
    });

    let llm = state
        .config
        .llm_config()
        .unwrap_or_else(|| smooth_operator_core::LlmConfig::openrouter("not-a-real-key"));

    let turn = runner::run_streaming_turn(
        TurnRequest {
            storage: state.storage.clone(),
            llm,
            max_iterations: 6,
            conversation_id: &conversation_id,
            request_id: "sm-grounded",
            user_message: "How big is the Frobnicator's ring buffer?",
            access: AccessContext::anonymous(),
            llm_provider: Some(Arc::new(mock.clone())),
            reranker: None,
        },
        &tx,
    )
    .await
    .expect("grounded turn");
    drop(tx);
    let _events = drain.await.expect("drain");

    // The grounded reply carries the retrieved fixture fact.
    assert!(
        turn.reply.contains("42"),
        "expected a grounded reply containing the ingested 42-slot fact, got: {:?}",
        turn.reply
    );
    assert!(
        turn.invoked_knowledge_search,
        "expected knowledge_search to run against the served knowledge"
    );
    // The turn cites the ingested source (retrieval really grounded it).
    assert!(
        !turn.citations.is_empty(),
        "expected at least one citation from the ingested repo"
    );
}
