//! Redis / Valkey [`Backplane`] — the horizontal scale-out seam.
//!
//! The default [`InMemoryBackplane`] only reaches sockets held by the current
//! process, so with more than one replica an event produced on pod A can't reach
//! a socket on pod B. [`RedisBackplane`] closes that gap **without changing the
//! trait or any call site**: it keeps a per-pod [`InMemoryBackplane`] for the
//! local registry + delivery, and adds a Redis pub/sub bus for cross-pod fan-out.
//!
//! Per call:
//! - [`attach`](Backplane::attach) / [`detach`](Backplane::detach) /
//!   [`associate`](Backplane::associate) are **local** — a connection lives on
//!   exactly one pod, so only that pod registers it.
//! - [`publish`](Backplane::publish) delivers to local sinks immediately
//!   (returning that count), then broadcasts a [`BackplaneEnvelope`] on the bus.
//!   Every pod's background subscriber re-resolves the envelope's [`Target`]
//!   against *its own* registry and delivers to its sockets — so the same
//!   `publish` call fans out across the whole fleet. The origin pod skips its own
//!   echo (it already delivered).
//!
//! This is the classic pub/sub fan-out (the shape Socket.IO's Redis adapter
//! uses): no shared connection registry, each pod authoritative for its own
//! sockets.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use redis::AsyncCommands;
use serde_json::Value;
use uuid::Uuid;

use smooth_operator::backplane::{
    Backplane, BackplaneEnvelope, InMemoryBackplane, LocalSink, Target,
};

/// Default pub/sub channel. Override via [`RedisBackplane::connect_on_channel`].
pub const DEFAULT_CHANNEL: &str = "smooth-operator:backplane";

/// A [`Backplane`] that fans `publish` out across pods over Redis/Valkey pub/sub.
pub struct RedisBackplane {
    /// Per-pod registry + local delivery. Shared with the subscriber task so
    /// remote envelopes deliver to the same sinks.
    local: Arc<InMemoryBackplane>,
    /// This pod's id — stamped on outgoing envelopes so we skip our own echo.
    pod_id: String,
    /// Cloneable multiplexed connection used for publishing.
    publisher: redis::aio::MultiplexedConnection,
    /// Channel events are broadcast on.
    channel: String,
    /// The background subscriber; aborted on drop.
    subscriber: tokio::task::JoinHandle<()>,
}

impl RedisBackplane {
    /// Connect to `url` (e.g. `redis://valkey:6379`) and fan out on
    /// [`DEFAULT_CHANNEL`].
    ///
    /// # Errors
    /// Returns an error if the URL is invalid or the connection / subscription
    /// can't be established.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_on_channel(url, DEFAULT_CHANNEL).await
    }

    /// Connect to `url`, broadcasting + subscribing on `channel` (lets two
    /// independent deployments share a Redis without crosstalk).
    ///
    /// # Errors
    /// Returns an error if the URL is invalid or the connection / subscription
    /// can't be established.
    pub async fn connect_on_channel(url: &str, channel: &str) -> Result<Self> {
        let client = redis::Client::open(url)?;
        let publisher = client.get_multiplexed_async_connection().await?;

        let pod_id = Uuid::new_v4().to_string();
        let local = Arc::new(InMemoryBackplane::new());

        // Subscribe BEFORE returning so the pod is receiving as soon as it exists.
        let mut pubsub = client.get_async_pubsub().await?;
        pubsub.subscribe(channel).await?;

        let sub_local = Arc::clone(&local);
        let sub_pod = pod_id.clone();
        let subscriber = tokio::spawn(async move {
            let mut stream = pubsub.on_message();
            while let Some(msg) = stream.next().await {
                let payload = msg.get_payload_bytes();
                let env: BackplaneEnvelope = match serde_json::from_slice(payload) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(error = %e, "backplane: undecodable envelope, dropping");
                        continue;
                    }
                };
                // Our own publish already delivered locally — don't double-deliver.
                if env.origin == sub_pod {
                    continue;
                }
                sub_local.publish(env.target, env.event).await;
            }
        });

        Ok(Self {
            local,
            pod_id,
            publisher,
            channel: channel.to_string(),
            subscriber,
        })
    }

    /// Number of connections attached **to this pod** (local registry only).
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.local.connection_count()
    }
}

impl Drop for RedisBackplane {
    fn drop(&mut self) {
        self.subscriber.abort();
    }
}

#[async_trait]
impl Backplane for RedisBackplane {
    async fn attach(&self, conn_id: &str, sink: LocalSink) {
        self.local.attach(conn_id, sink).await;
    }

    async fn detach(&self, conn_id: &str) {
        self.local.detach(conn_id).await;
    }

    async fn associate(&self, conn_id: &str, target: Target) {
        self.local.associate(conn_id, target).await;
    }

    async fn publish(&self, target: Target, event: Value) -> usize {
        // Deliver to this pod's sockets now (the returned count).
        let n = self.local.publish(target.clone(), event.clone()).await;

        // Fan out to every other pod.
        let envelope = BackplaneEnvelope {
            origin: self.pod_id.clone(),
            target,
            event,
        };
        match serde_json::to_vec(&envelope) {
            Ok(payload) => {
                let mut conn = self.publisher.clone();
                if let Err(e) = conn
                    .publish::<_, _, ()>(self.channel.as_str(), payload)
                    .await
                {
                    tracing::warn!(error = %e, "backplane: redis publish failed; cross-pod delivery skipped");
                }
            }
            Err(e) => tracing::warn!(error = %e, "backplane: envelope serialize failed"),
        }
        n
    }
}
