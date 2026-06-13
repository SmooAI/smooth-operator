//! Backplane end-to-end (SMOODEV-1891): a connection registers with the
//! backplane on connect + its session on `create_conversation_session`, so an
//! out-of-band publisher — standing in for any non-AI service (job status,
//! notifications) — can push an event to that session and have it arrive over
//! the client's WebSocket. This is the realtime pub/sub plug point.

mod common;

use std::sync::Arc;

use serde_json::json;

use smooth_operator::backplane::{Backplane, InMemoryBackplane, Target};
use smooth_operator_server::config::ServerConfig;
use smooth_operator_server::server::{build_state, router};

fn keyless_config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "claude-haiku-4-5".into(),
        seed_kb: true,
        max_iterations: 4,
        max_tokens: 128,
        storage: smooth_operator_server::config::StorageBackend::Memory,
        widget_auth_strict: false,
    }
}

/// Boot the server holding our own [`InMemoryBackplane`] so the test can publish
/// to it directly (as an external service would).
async fn boot_with_backplane(bp: Arc<InMemoryBackplane>) -> String {
    let state = build_state(keyless_config()).with_backplane(bp);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("ws://{addr}/ws")
}

#[tokio::test]
async fn publishing_to_a_session_reaches_the_connected_client() {
    let bp = Arc::new(InMemoryBackplane::new());
    let url = boot_with_backplane(bp.clone()).await;
    let mut client = common::connect(&url).await;

    // Create a session; capture its id from the descriptor.
    common::send_json(&mut client, &json!({ "action": "create_conversation_session", "requestId": "cs-1", "agentId": "11111111-1111-4111-8111-111111111111" })).await;
    let created = common::recv_json(&mut client).await;
    assert_eq!(created["type"], "immediate_response", "got: {created}");
    let session_id = created["data"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // An out-of-band publisher pushes an event to the session.
    let event = json!({ "type": "job_status", "data": { "jobId": "abc", "state": "complete" } });
    let delivered = bp.publish(Target::Session(session_id), event.clone()).await;
    assert_eq!(delivered, 1, "expected exactly one local delivery");

    // It arrives over the client's WebSocket.
    let received = common::recv_json(&mut client).await;
    assert_eq!(
        received, event,
        "client should receive the published event verbatim"
    );
}

#[tokio::test]
async fn detach_on_disconnect_stops_delivery() {
    let bp = Arc::new(InMemoryBackplane::new());
    let url = boot_with_backplane(bp.clone()).await;

    let session_id = {
        let mut client = common::connect(&url).await;
        common::send_json(&mut client, &json!({ "action": "create_conversation_session", "requestId": "cs-2", "agentId": "11111111-1111-4111-8111-111111111111" })).await;
        let created = common::recv_json(&mut client).await;
        let sid = created["data"]["sessionId"].as_str().unwrap().to_string();
        // client drops here → the server's read loop ends → detach runs.
        sid
    };

    // Give the server a moment to observe the close + detach.
    for _ in 0..50 {
        if bp.connection_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        bp.connection_count(),
        0,
        "connection should be detached after disconnect"
    );
    assert_eq!(
        bp.publish(Target::Session(session_id), json!(1)).await,
        0,
        "no delivery to a gone connection"
    );
}
