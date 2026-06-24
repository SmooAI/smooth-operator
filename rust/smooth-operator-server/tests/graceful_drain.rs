//! Graceful SIGTERM/shutdown drain test.
//!
//! Proves the shape the k8s pod-termination drain relies on: cancelling the
//! shared [`CancellationToken`] on [`AppState`] makes a live per-connection
//! reader loop break, run the post-loop `Backplane::detach`, and leave the
//! backplane registry empty — i.e. no in-flight turn dropped silently AND no
//! stale registry entry left behind on scale-down/deploy.
//!
//! The full SIGTERM → `axum::serve(...).with_graceful_shutdown` wiring lives in
//! `server::run`; here we drive the same token the serve loop would cancel, so
//! the connection-level drain behaviour is exercised end-to-end over a real
//! WebSocket without sending an actual process signal in-test.

mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use smooth_operator::backplane::InMemoryBackplane;
use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::server::{build_state, router};

fn keyless_config() -> ServerConfig {
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
    }
}

/// Poll `cond` until it holds or the deadline elapses; fail the test otherwise.
async fn wait_until(label: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {label}");
}

#[tokio::test]
async fn cancelling_shutdown_drains_connection_and_detaches() {
    // Build state with an INSPECTABLE in-memory backplane and a shared shutdown
    // token (the same token the serve loop cancels on SIGTERM). We keep handles
    // to both so we can drive + observe the drain.
    let backplane = Arc::new(InMemoryBackplane::new());
    let shutdown = CancellationToken::new();
    let state = build_state(keyless_config())
        .with_backplane(backplane.clone())
        .with_shutdown(shutdown.clone());

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let url = format!("ws://{addr}/ws");
    let mut client = common::connect(&url).await;

    // Drive a ping → pong so the per-connection reader loop is provably live and
    // the connection has attached its sink to the backplane.
    common::send_json(&mut client, &json!({ "action": "ping", "requestId": "p1" })).await;
    let pong = common::recv_json(&mut client).await;
    assert_eq!(pong["type"], "pong", "expected pong, got: {pong}");

    // The live connection registered exactly one sink with the backplane.
    wait_until("connection attached", || backplane.connection_count() == 1).await;

    // Fire the shutdown signal: this is what `server::run`'s graceful-shutdown
    // future does on SIGTERM/ctrl_c. The reader loop's `select!` must observe the
    // cancellation, break, and run the post-loop `detach`.
    shutdown.cancel();

    // Drain happened: the connection detached, leaving NO stale registry entry.
    wait_until("connection detached after shutdown", || {
        backplane.connection_count() == 0
    })
    .await;

    server.abort();
}

#[tokio::test]
async fn fresh_state_token_is_not_cancelled() {
    // A default-constructed AppState must carry a fresh, never-cancelled token so
    // the `/ws` path and existing tests are unaffected until a serve path cancels
    // it. This guards against accidentally sharing a pre-cancelled token.
    let state = build_state(keyless_config());
    assert!(
        !state.shutdown.is_cancelled(),
        "a fresh AppState must not start cancelled"
    );

    // A clone shares the SAME cancellation state (the property the fan-out relies
    // on): cancelling one observable from the other.
    let clone = state.clone();
    assert!(!clone.shutdown.is_cancelled());
    state.shutdown.cancel();
    assert!(
        clone.shutdown.is_cancelled(),
        "cloned token must observe cancellation from the original"
    );
}
