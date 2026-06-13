//! Connection backplane: the scale-out + event-delivery seam for the WebSocket
//! server.
//!
//! The reference server is single-process — each connection's outbound sink is a
//! per-connection in-process channel, with no registry and no way to reach a
//! connection from outside its own read loop. That blocks two things we need:
//!
//! 1. **Horizontal scale-out.** With >1 replica, an agent turn (or any event)
//!    produced on pod A must reach a socket held by pod B.
//! 2. **Non-AI realtime.** Other parts of a host system (job status, ingestion
//!    progress, notifications) want to push events to a connected client without
//!    going through an agent turn.
//!
//! The [`Backplane`] trait is the seam for both. Each connection's local sink is
//! [`attach`](Backplane::attach)ed on connect and [`associate`](Backplane::associate)d
//! with targets (its session / user / org / agent) as they're learned;
//! [`publish`](Backplane::publish) delivers an event to **every connection for a
//! target, wherever its pod is**.
//!
//! - [`InMemoryBackplane`] (the default) keeps a local registry and delivers
//!   straight to local sinks — single process, no external services.
//! - A Redis / NATS impl (separate crate work) publishes to the bus, and each
//!   pod's subscriber delivers to *its* local sinks — making the same `publish`
//!   call fan out across the fleet.
//!
//! This module is a public **mechanism**: a host plugs its chosen impl in via
//! `AppState::with_backplane(...)`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::Value;

/// A connection's local delivery sink: given an event, write it to that
/// connection's socket. Runtime-agnostic — the server wraps its outbound channel
/// in a closure, so the backplane (and this lib) take no async-runtime
/// dependency.
pub type LocalSink = Arc<dyn Fn(Value) + Send + Sync>;

/// A delivery target: a single connection, or every connection associated with a
/// session / user / org / agent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// One specific connection.
    Connection(String),
    /// Every connection in a conversation session.
    Session(String),
    /// Every connection for a user.
    User(String),
    /// Every connection for an org/tenant.
    Org(String),
    /// Every connection talking to an agent.
    Agent(String),
}

/// The connection backplane: a per-pod sink registry + cross-pod event delivery.
///
/// Implementations must be cheap to clone behind an `Arc` and safe to share
/// across every connection task.
#[async_trait]
pub trait Backplane: Send + Sync {
    /// Attach a connection's local outbound sink — this pod owns the socket.
    /// Idempotent re-attach replaces the sink. The connection is always
    /// reachable by [`Target::Connection`] with its own id.
    async fn attach(&self, conn_id: &str, sink: LocalSink);

    /// Detach a connection and drop all of its target associations and its local
    /// sink (call on disconnect).
    async fn detach(&self, conn_id: &str);

    /// Associate a connection with a target (idempotent). Learned over the
    /// connection's life: the session at `create_conversation_session`, the
    /// user/org from auth, etc.
    async fn associate(&self, conn_id: &str, target: Target);

    /// Deliver `event` to every connection associated with `target`. Returns the
    /// number of **local** deliveries made on this pod; cross-pod impls also fan
    /// the event out to other pods (whose local deliveries this count omits).
    async fn publish(&self, target: Target, event: Value) -> usize;
}

/// Single-process [`Backplane`]: an in-memory registry with direct local
/// delivery. The default — keeps the server runnable standalone. Multi-pod
/// deployments install a Redis / NATS impl instead.
#[derive(Default)]
pub struct InMemoryBackplane {
    inner: RwLock<Registry>,
}

#[derive(Default)]
struct Registry {
    /// conn id → its local delivery sink.
    sinks: HashMap<String, LocalSink>,
    /// conn id → the targets it's associated with (for cleanup on detach).
    conn_targets: HashMap<String, HashSet<Target>>,
    /// target → the conn ids associated with it (for publish fan-out).
    target_conns: HashMap<Target, HashSet<String>>,
}

impl InMemoryBackplane {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Test/inspection helper: number of attached connections.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.inner.read().expect("backplane lock").sinks.len()
    }
}

#[async_trait]
impl Backplane for InMemoryBackplane {
    async fn attach(&self, conn_id: &str, sink: LocalSink) {
        let mut r = self.inner.write().expect("backplane lock");
        r.sinks.insert(conn_id.to_string(), sink);
        // Always reachable by its own connection id.
        let self_target = Target::Connection(conn_id.to_string());
        r.conn_targets
            .entry(conn_id.to_string())
            .or_default()
            .insert(self_target.clone());
        r.target_conns
            .entry(self_target)
            .or_default()
            .insert(conn_id.to_string());
    }

    async fn detach(&self, conn_id: &str) {
        let mut r = self.inner.write().expect("backplane lock");
        r.sinks.remove(conn_id);
        if let Some(targets) = r.conn_targets.remove(conn_id) {
            for t in targets {
                let empty = if let Some(set) = r.target_conns.get_mut(&t) {
                    set.remove(conn_id);
                    set.is_empty()
                } else {
                    false
                };
                if empty {
                    r.target_conns.remove(&t);
                }
            }
        }
    }

    async fn associate(&self, conn_id: &str, target: Target) {
        let mut r = self.inner.write().expect("backplane lock");
        r.conn_targets
            .entry(conn_id.to_string())
            .or_default()
            .insert(target.clone());
        r.target_conns
            .entry(target)
            .or_default()
            .insert(conn_id.to_string());
    }

    async fn publish(&self, target: Target, event: Value) -> usize {
        let r = self.inner.read().expect("backplane lock");
        let Some(conns) = r.target_conns.get(&target) else {
            return 0;
        };
        let mut delivered = 0;
        for conn in conns {
            if let Some(sink) = r.sinks.get(conn) {
                // The sink closure is non-blocking (it pushes onto the
                // connection's channel); safe to call under the read lock.
                sink(event.clone());
                delivered += 1;
            }
        }
        delivered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::mpsc::{channel, Receiver};

    /// A runtime-agnostic test sink: a [`LocalSink`] closure feeding a std channel.
    fn sink() -> (LocalSink, Receiver<Value>) {
        let (tx, rx) = channel::<Value>();
        (
            Arc::new(move |v| {
                let _ = tx.send(v);
            }),
            rx,
        )
    }

    #[tokio::test]
    async fn publishes_to_a_session_across_its_connections() {
        let bp = InMemoryBackplane::new();
        let (sa, rx_a) = sink();
        let (sb, rx_b) = sink();
        bp.attach("conn-a", sa).await;
        bp.attach("conn-b", sb).await;
        bp.associate("conn-a", Target::Session("s1".into())).await;
        bp.associate("conn-b", Target::Session("s1".into())).await;

        let n = bp
            .publish(Target::Session("s1".into()), json!({"hi": 1}))
            .await;
        assert_eq!(n, 2);
        assert_eq!(rx_a.try_recv().unwrap(), json!({"hi": 1}));
        assert_eq!(rx_b.try_recv().unwrap(), json!({"hi": 1}));
    }

    #[tokio::test]
    async fn publishes_to_a_single_connection() {
        let bp = InMemoryBackplane::new();
        let (s, rx) = sink();
        bp.attach("conn-1", s).await;
        let n = bp
            .publish(Target::Connection("conn-1".into()), json!("ping"))
            .await;
        assert_eq!(n, 1);
        assert_eq!(rx.try_recv().unwrap(), json!("ping"));
    }

    #[tokio::test]
    async fn unknown_target_delivers_to_nobody() {
        let bp = InMemoryBackplane::new();
        assert_eq!(
            bp.publish(Target::Session("nope".into()), json!(1)).await,
            0
        );
    }

    #[tokio::test]
    async fn detach_removes_sink_and_associations() {
        let bp = InMemoryBackplane::new();
        let (s, _rx) = sink();
        bp.attach("conn-x", s).await;
        bp.associate("conn-x", Target::User("u1".into())).await;
        assert_eq!(bp.connection_count(), 1);

        bp.detach("conn-x").await;
        assert_eq!(bp.connection_count(), 0);
        // Its targets no longer resolve to it.
        assert_eq!(bp.publish(Target::User("u1".into()), json!(1)).await, 0);
        assert_eq!(
            bp.publish(Target::Connection("conn-x".into()), json!(1))
                .await,
            0
        );
    }

    #[tokio::test]
    async fn a_connection_can_serve_multiple_targets() {
        let bp = InMemoryBackplane::new();
        let (s, rx) = sink();
        bp.attach("c", s).await;
        bp.associate("c", Target::Session("s".into())).await;
        bp.associate("c", Target::Org("o".into())).await;
        assert_eq!(
            bp.publish(Target::Org("o".into()), json!("org-event"))
                .await,
            1
        );
        assert_eq!(
            bp.publish(Target::Session("s".into()), json!("sess-event"))
                .await,
            1
        );
        assert_eq!(rx.try_recv().unwrap(), json!("org-event"));
        assert_eq!(rx.try_recv().unwrap(), json!("sess-event"));
    }
}
