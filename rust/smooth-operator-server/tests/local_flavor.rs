//! Local deployment flavor — hermetic boot test (NO external services).
//!
//! Proves the embeddable [`LocalServer`](smooth_operator_server::local::LocalServer)
//! boots a working WebSocket server with **everything in-memory** (in-memory
//! storage, in-memory backplane, no auth) and accepts a connection + a `ping`
//! over the canonical protocol — no Postgres, no Redis, no NATS, no AWS, no
//! gateway key. This is the "third deployment option" the smooth daemon embeds.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use smooth_operator_server::local::LocalServer;

#[tokio::test]
async fn local_server_boots_and_answers_ping() {
    // Boot the local flavor in-process on an ephemeral port — no env, no creds,
    // no external services. This is exactly the `serve_local` path minus the
    // run-forever foreground loop.
    let server = LocalServer::builder()
        .addr("127.0.0.1:0".parse().unwrap())
        .seed_kb(true)
        .spawn()
        .await
        .expect("local server should boot with in-memory everything");

    // The handle reports the real bound port (resolved from port 0).
    let addr = server.addr();
    assert_ne!(addr.port(), 0, "ephemeral port must be resolved: {addr}");

    // Connect a real WebSocket client to the canonical `/ws` endpoint.
    let (mut client, _resp) = connect_async(server.ws_url())
        .await
        .expect("connect to local /ws");

    // Drive `ping` → `pong` over the wire protocol.
    client
        .send(WsMessage::Text(
            json!({ "action": "ping", "requestId": "local-ping-1" })
                .to_string()
                .into(),
        ))
        .await
        .expect("send ping");

    let frame = tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .expect("recv timed out")
        .expect("stream ended")
        .expect("ws error");
    let ev: Value = match frame {
        WsMessage::Text(t) => serde_json::from_str(&t).expect("parse json event"),
        other => panic!("expected text frame, got: {other:?}"),
    };

    assert_eq!(ev["type"], "pong", "expected pong, got: {ev}");
    assert_eq!(ev["requestId"], "local-ping-1");
    assert!(
        ev["timestamp"].is_i64(),
        "pong must carry a timestamp: {ev}"
    );

    // Clean graceful shutdown joins the background task.
    drop(client);
    server.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn local_server_creates_session_with_no_creds() {
    // The local flavor speaks the full protocol with zero credentials — a
    // session can be created without a gateway key or any auth.
    let server = LocalServer::builder()
        .addr("127.0.0.1:0".parse().unwrap())
        .spawn()
        .await
        .expect("local server should boot");

    let (mut client, _resp) = connect_async(server.ws_url())
        .await
        .expect("connect to local /ws");

    let agent_id = uuid::Uuid::new_v4().to_string();
    client
        .send(WsMessage::Text(
            json!({
                "action": "create_conversation_session",
                "requestId": "local-cs-1",
                "agentId": agent_id,
                "userName": "Local Visitor",
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send create_conversation_session");

    let frame = tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .expect("recv timed out")
        .expect("stream ended")
        .expect("ws error");
    let ev: Value = match frame {
        WsMessage::Text(t) => serde_json::from_str(&t).expect("parse json event"),
        other => panic!("expected text frame, got: {other:?}"),
    };

    assert_eq!(ev["type"], "immediate_response", "got: {ev}");
    assert_eq!(ev["status"], 200);
    assert_eq!(ev["data"]["agentId"], agent_id);
    assert!(
        uuid::Uuid::parse_str(ev["data"]["sessionId"].as_str().unwrap()).is_ok(),
        "sessionId must be a UUID: {ev}"
    );

    drop(client);
    server.shutdown().await.expect("clean shutdown");
}
