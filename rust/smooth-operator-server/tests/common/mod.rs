//! Shared test helpers: boot the server in-process on an ephemeral port and
//! return a connected WebSocket client.

// Shared across several test binaries; not every binary uses every helper.
#![allow(dead_code)]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use smooth_operator_server::config::ServerConfig;
use smooth_operator_server::server::{build_state, router};
use smooth_operator_server::state::AppState;

/// A connected client WS stream.
pub type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Boot the server on `127.0.0.1:0` (ephemeral port) with the given config and
/// return the bound `ws://…/ws` URL. The server runs in a background task for
/// the lifetime of the test process.
pub async fn boot(config: ServerConfig) -> String {
    boot_state(build_state(config)).await
}

/// Boot the server from a **prebuilt** [`AppState`] (so a test can install a
/// builder-configured collaborator — e.g. a `MockLlmClient` via
/// [`AppState::with_chat_provider`](smooth_operator_server::state::AppState::with_chat_provider)
/// for the scenario-parity corpus). Returns the bound `ws://…/ws` URL.
pub async fn boot_state(state: AppState) -> String {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("ws://{addr}/ws")
}

/// Connect a WS client to `url`.
pub async fn connect(url: &str) -> Client {
    let (ws, _resp) = connect_async(url).await.expect("connect ws");
    ws
}

/// Send a JSON value as a text frame.
pub async fn send_json(client: &mut Client, value: &Value) {
    client
        .send(WsMessage::Text(value.to_string().into()))
        .await
        .expect("send frame");
}

/// Receive the next JSON event, with a timeout so a hung server fails the test
/// instead of blocking forever.
pub async fn recv_json(client: &mut Client) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(30), client.next())
        .await
        .expect("recv timed out")
        .expect("stream ended")
        .expect("ws error");
    match frame {
        WsMessage::Text(t) => serde_json::from_str(&t).expect("parse json event"),
        WsMessage::Close(_) => panic!("connection closed unexpectedly"),
        other => panic!("expected text frame, got: {other:?}"),
    }
}

/// Receive events until one with `type == event_type` arrives (or a long
/// timeout). Returns the matching event; collects all intermediate events into
/// `seen`. Errors are returned as soon as seen (so a test never hangs waiting
/// for a terminal event that won't come).
pub async fn recv_until(
    client: &mut Client,
    event_type: &str,
    seen: &mut Vec<Value>,
    overall_timeout: Duration,
) -> Value {
    let deadline = tokio::time::Instant::now() + overall_timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for '{event_type}'; saw: {:?}",
            seen.iter().map(|e| e["type"].clone()).collect::<Vec<_>>()
        );
        let frame = tokio::time::timeout(remaining, client.next())
            .await
            .expect("recv timed out")
            .expect("stream ended")
            .expect("ws error");
        let ev: Value = match frame {
            WsMessage::Text(t) => serde_json::from_str(&t).expect("parse json event"),
            WsMessage::Close(_) => panic!("connection closed before '{event_type}'"),
            _ => continue,
        };
        let ty = ev["type"].as_str().unwrap_or_default().to_string();
        seen.push(ev.clone());
        if ty == event_type {
            return ev;
        }
        if ty == "error" && event_type != "error" {
            panic!("received error event while waiting for '{event_type}': {ev}");
        }
    }
}
