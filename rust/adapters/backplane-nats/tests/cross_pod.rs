//! Cross-pod fan-out (SMOODEV-1892): two `NatsBackplane` instances sharing one
//! NATS broker stand in for two pods. A socket lives on pod B; an event
//! published on pod A still reaches pod B's socket over the bus — horizontal
//! scale-out.
//!
//! Requires a Docker daemon for the throwaway NATS. Skips (returns Ok) when
//! Docker is unavailable so CI without one stays green (mirrors the PG adapter).
//!
//! Both behaviours live in one test sharing a single container — one
//! container-per-binary keeps the suite robust under parallel test execution.

use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage};

use smooth_operator::backplane::{Backplane, LocalSink, Target};
use smooth_operator_adapter_backplane_nats::NatsBackplane;

/// Start a throwaway `nats:2.10-alpine`. Returns `Ok(None)` if Docker is
/// unavailable so the caller skips rather than fails.
async fn start_nats() -> anyhow::Result<Option<(ContainerAsync<GenericImage>, String)>> {
    let image = GenericImage::new("nats", "2.10-alpine")
        .with_exposed_port(4222.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Server is ready"));

    match image.start().await {
        Ok(node) => {
            let host = node.get_host().await?;
            let port = node.get_host_port_ipv4(4222).await?;
            Ok(Some((node, format!("nats://{host}:{port}"))))
        }
        Err(e) => {
            eprintln!("SKIP: could not start nats container (Docker unavailable?): {e}");
            Ok(None)
        }
    }
}

fn sink() -> (LocalSink, Receiver<Value>) {
    let (tx, rx) = channel::<Value>();
    (
        Arc::new(move |v| {
            let _ = tx.send(v);
        }),
        rx,
    )
}

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
async fn nats_backplane_fans_out_across_pods() -> anyhow::Result<()> {
    let Some((_node, url)) = start_nats().await? else {
        return Ok(()); // Docker unavailable — skip.
    };

    let pod_a = NatsBackplane::connect(&url).await?;
    let pod_b = NatsBackplane::connect(&url).await?;

    let (s, rx) = sink();
    pod_b.attach("conn-b", s).await;
    pod_b
        .associate("conn-b", Target::Agent("agent-9".into()))
        .await;
    assert_eq!(pod_a.connection_count(), 0);
    assert_eq!(pod_b.connection_count(), 1);

    // Let pod B's subscriber task start polling before we publish. In prod the
    // per-pod subscriber is live at boot, long before any socket attaches, so
    // this only aligns the test with that ordering — it masks no real loss.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // (1) Cross-pod delivery.
    let local = pod_a
        .publish(
            Target::Agent("agent-9".into()),
            json!({"kind": "notify", "n": 1}),
        )
        .await;
    assert_eq!(local, 0, "pod A delivers to 0 local sockets");
    let got = recv_within(&rx).expect("pod B's socket should receive the cross-pod event");
    assert_eq!(got, json!({"kind": "notify", "n": 1}));

    // (2) No double-deliver on the owning pod.
    let n = pod_b
        .publish(Target::Agent("agent-9".into()), json!("once"))
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
