//! The axum WebSocket server: one `/ws` endpoint, one task per connection.
//!
//! Per connection we split the socket and run two tasks joined by an
//! `UnboundedSender<serde_json::Value>` outbound sink:
//!
//! - a **writer** that drains the sink and writes each event as a JSON text
//!   frame, and
//! - a **reader** that reads inbound frames and dispatches them via
//!   [`crate::handler::handle_frame`], passing the sink so handlers (including
//!   the streaming `send_message`) can emit events as they happen.
//!
//! Using a sink channel (instead of writing directly from the handler) is what
//! lets a streaming turn fire many `stream_token` events from inside the agent
//! loop while the connection is still reading.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;

use smooth_operator::{Document, DocumentType};
use smooth_operator_agent_adapter_memory::InMemoryStorageAdapter;

use crate::config::ServerConfig;
use crate::handler;
use crate::state::AppState;

/// Build the axum [`Router`] for the given application state. Exposed so tests
/// can boot the server in-process.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

/// Build an [`AppState`] over a fresh in-memory adapter, seeding the knowledge
/// base when `config.seed_kb` is set. Exposed for tests + the binary.
#[must_use]
pub fn build_state(config: ServerConfig) -> AppState {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    if config.seed_kb {
        seed_knowledge(storage.as_ref());
    }
    AppState::new(storage, config)
}

/// Seed a couple of distinctive demo docs so knowledge-grounded E2E is
/// deterministic. The 17-day return window is deliberately unusual so an
/// ungrounded answer can't accidentally match it.
pub fn seed_knowledge(storage: &InMemoryStorageAdapter) {
    let kb = smooth_operator_agent_core::adapter::StorageAdapter::knowledge(storage);
    let _ = kb.ingest(Document::new(
        "SmooAI's return window is exactly 17 days from delivery. Returns after 17 days are not accepted.",
        "policies/returns.md",
        DocumentType::Documentation,
    ));
    let _ = kb.ingest(Document::new(
        "SmooAI standard shipping takes 5 to 7 business days. Expedited shipping takes 2 business days.",
        "policies/shipping.md",
        DocumentType::Documentation,
    ));
}

/// Bind on `<SMOOTH_AGENT_BIND>:<port>` (default loopback) and serve until the
/// process is killed. Returns the bound [`TcpListener`] + the router, used by
/// both the binary and tests (tests bind port 0 for an ephemeral port).
///
/// # Errors
/// Returns an error if the TCP bind fails.
pub async fn bind(config: ServerConfig) -> Result<(TcpListener, Router)> {
    let ip: std::net::IpAddr = config
        .bind
        .parse()
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let addr = SocketAddr::new(ip, config.port);
    let state = build_state(config);
    let app = router(state);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding WebSocket server on {addr}"))?;
    Ok((listener, app))
}

/// Run the server to completion (blocks). Logs a single listening line.
///
/// # Errors
/// Returns an error if binding or serving fails.
pub async fn run(config: ServerConfig) -> Result<()> {
    let has_llm = config.has_llm();
    let model = config.model.clone();
    let gateway = config.gateway_url.clone();
    let (listener, app) = bind(config).await?;
    let local = listener.local_addr().context("local addr")?;

    tracing::info!(
        %local,
        endpoint = "/ws",
        %model,
        %gateway,
        llm_enabled = has_llm,
        "smooth-agent-server listening"
    );
    // Also print to stdout so the run-confirmation check is unambiguous without
    // a tracing subscriber filter.
    println!(
        "smooth-agent-server listening on ws://{local}/ws (model={model}, llm_enabled={has_llm})"
    );

    axum::serve(listener, app)
        .await
        .context("serving WebSocket connections")?;
    Ok(())
}

/// Axum handler: upgrade an HTTP request on `/ws` to a WebSocket.
async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| connection_loop(socket, state))
}

/// Drive one WebSocket connection: split into reader + writer, joined by an
/// outbound event sink.
async fn connection_loop(socket: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();

    // Writer: drain the sink and write each event as a JSON text frame.
    let writer = tokio::spawn(async move {
        while let Some(event) = sink_rx.recv().await {
            let text = match serde_json::to_string(&event) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ws_tx.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Reader: dispatch inbound frames. Handlers emit events via `sink_tx`.
    while let Some(frame) = ws_rx.next().await {
        match frame {
            Ok(Message::Text(text)) => {
                handler::handle_frame(&state, text.as_str(), &sink_tx).await;
            }
            Ok(Message::Binary(_)) => {
                let _ = sink_tx.send(crate::protocol::error(
                    None,
                    "VALIDATION_ERROR",
                    "binary frames are not supported; send JSON text frames",
                ));
            }
            Ok(Message::Close(_)) => break,
            // Ping/Pong control frames are handled by axum automatically.
            Ok(_) => {}
            Err(_) => break,
        }
    }

    // Reader finished → drop the sink so the writer task exits.
    drop(sink_tx);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use smooth_operator_agent_core::adapter::StorageAdapter;

    #[test]
    fn seeded_kb_returns_17_day_fact() {
        let storage = InMemoryStorageAdapter::new();
        seed_knowledge(&storage);
        let results = storage
            .knowledge()
            .query("return window policy", 3)
            .expect("query");
        assert!(
            results.iter().any(|r| r.chunk.contains("17")),
            "expected seeded 17-day fact, got: {results:?}"
        );
    }

    #[tokio::test]
    async fn build_state_without_key_has_no_llm() {
        let cfg = ServerConfig {
            bind: "127.0.0.1".into(),
            port: 0,
            gateway_url: "https://example.test/v1".into(),
            gateway_key: None,
            model: "m".into(),
            seed_kb: true,
            max_iterations: 4,
            max_tokens: 128,
        };
        let state = build_state(cfg);
        assert!(!state.config.has_llm());
    }
}
