//! Cross-pod fan-out (SMOODEV-1892): two `RedisBackplane` instances sharing one
//! Redis stand in for two pods. A connection (sink) lives on pod B; an event
//! published on pod A — where that connection does NOT exist locally — still
//! reaches pod B's sink over the bus. This is horizontal scale-out: the same
//! `publish` call delivers to a socket on another replica.
//!
//! Requires a Docker daemon for the throwaway Redis. Skips (returns Ok) when
//! Docker is unavailable so CI without one stays green (mirrors the PG adapter).
//!
//! Both behaviours live in one test sharing a single container — the two
//! scenarios don't need separate containers, and one-container-per-binary keeps
//! the suite robust under parallel test execution.

use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage};

use smooth_operator::backplane::{Backplane, LocalSink, Target};
use smooth_operator_adapter_backplane_redis::RedisBackplane;

/// Start a throwaway `redis:7-alpine`. Returns `Ok(None)` if Docker is
/// unavailable so the caller skips rather than fails.
async fn start_redis() -> anyhow::Result<Option<(ContainerAsync<GenericImage>, String)>> {
    let image = GenericImage::new("redis", "7-alpine")
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"));

    match image.start().await {
        Ok(node) => {
            let host = node.get_host().await?;
            let port = node.get_host_port_ipv4(6379).await?;
            Ok(Some((node, format!("redis://{host}:{port}"))))
        }
        Err(e) => {
            eprintln!("SKIP: could not start redis container (Docker unavailable?): {e}");
            Ok(None)
        }
    }
}

/// A test sink feeding a std channel (runtime-agnostic, like the lib's own).
fn sink() -> (LocalSink, Receiver<Value>) {
    let (tx, rx) = channel::<Value>();
    (
        Arc::new(move |v| {
            let _ = tx.send(v);
        }),
        rx,
    )
}

/// Poll a std channel for up to ~2s (the bus round-trip is async).
fn recv_within(rx: &Receiver<Value>) -> Option<Value> {
    for _ in 0..200 {
        if let Ok(v) = rx.try_recv() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redis_backplane_fans_out_across_pods() -> anyhow::Result<()> {
    let Some((_node, url)) = start_redis().await? else {
        return Ok(()); // Docker unavailable — skip.
    };

    // Two pods on the same Redis.
    let pod_a = RedisBackplane::connect(&url).await?;
    let pod_b = RedisBackplane::connect(&url).await?;

    // A connection for session "s1" lives on pod B only.
    let (s, rx) = sink();
    pod_b.attach("conn-b", s).await;
    pod_b
        .associate("conn-b", Target::Session("s1".into()))
        .await;
    assert_eq!(pod_a.connection_count(), 0, "pod A holds no sockets");
    assert_eq!(pod_b.connection_count(), 1);

    // Let pod B's subscriber task start polling before we publish. In prod the
    // per-pod subscriber is live at boot, long before any socket attaches, so
    // this only aligns the test with that ordering — it masks no real loss.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // (1) Cross-pod: publish on pod A — it has no local socket for s1 (0 local
    // deliveries) but the event must still cross the bus to pod B's socket.
    let local = pod_a
        .publish(
            Target::Session("s1".into()),
            json!({"kind": "job_status", "state": "done"}),
        )
        .await;
    assert_eq!(local, 0, "pod A delivers to 0 local sockets");
    let got = recv_within(&rx).expect("pod B's socket should receive the cross-pod event");
    assert_eq!(got, json!({"kind": "job_status", "state": "done"}));

    // (2) No double-deliver: publishing on the pod that HOLDS the socket delivers
    // exactly once (locally); the echoed bus message must be skipped.
    let n = pod_b
        .publish(Target::Session("s1".into()), json!("once"))
        .await;
    assert_eq!(n, 1, "one local delivery on the owning pod");
    assert_eq!(recv_within(&rx), Some(json!("once")));
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        rx.try_recv().is_err(),
        "no double delivery from the bus echo"
    );

    Ok(())
}
