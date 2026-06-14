//! NATS [`Backplane`] — cross-pod scale-out + a shared event bus.
//!
//! Same shape as the Redis adapter (a per-pod [`InMemoryBackplane`] for local
//! registry + delivery, plus a bus for cross-pod fan-out) but over NATS subjects.
//! NATS is attractive here beyond raw fan-out: queue groups, JetStream
//! persistence/replay, and the fact that the same broker doubles as the
//! platform's multi-channel event bus — so non-AI publishers (job status,
//! ingestion progress, notifications) and other services can share it.
//!
//! Per call:
//! - [`attach`](Backplane::attach) / [`detach`](Backplane::detach) /
//!   [`associate`](Backplane::associate) are **local** (a connection lives on one
//!   pod).
//! - [`publish`](Backplane::publish) delivers to local sinks immediately
//!   (returning that count), then publishes a [`BackplaneEnvelope`] on the
//!   subject. Every pod's subscriber re-resolves the [`Target`] against its own
//!   registry; the origin pod skips its own echo.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use uuid::Uuid;

use smooth_operator::backplane::{
    Backplane, BackplaneEnvelope, InMemoryBackplane, LocalSink, Target,
};

/// Default subject. Override via [`NatsBackplane::connect_on_subject`].
pub const DEFAULT_SUBJECT: &str = "smooth-operator.backplane";

/// A [`Backplane`] that fans `publish` out across pods over NATS.
pub struct NatsBackplane {
    /// Per-pod registry + local delivery, shared with the subscriber task.
    local: Arc<InMemoryBackplane>,
    /// This pod's id — stamped on outgoing envelopes so we skip our own echo.
    pod_id: String,
    /// NATS client (cheap to clone; used for publishing).
    client: async_nats::Client,
    /// Subject events are published on.
    subject: String,
    /// The background subscriber; aborted on drop.
    subscriber: tokio::task::JoinHandle<()>,
}

impl NatsBackplane {
    /// Connect to `url` (e.g. `nats://nats:4222`) and fan out on
    /// [`DEFAULT_SUBJECT`].
    ///
    /// # Errors
    /// Returns an error if the connection or subscription can't be established.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_on_subject(url, DEFAULT_SUBJECT).await
    }

    /// Connect to `url`, broadcasting + subscribing on `subject` (lets two
    /// independent deployments share a broker without crosstalk).
    ///
    /// # Errors
    /// Returns an error if the connection or subscription can't be established.
    pub async fn connect_on_subject(url: &str, subject: &str) -> Result<Self> {
        let client = async_nats::connect(url).await?;

        let pod_id = Uuid::new_v4().to_string();
        let local = Arc::new(InMemoryBackplane::new());

        // Subscribe BEFORE returning so the pod receives as soon as it exists.
        let mut sub = client.subscribe(subject.to_string()).await?;

        let sub_local = Arc::clone(&local);
        let sub_pod = pod_id.clone();
        let subscriber = tokio::spawn(async move {
            while let Some(msg) = sub.next().await {
                let env: BackplaneEnvelope = match serde_json::from_slice(&msg.payload) {
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
            client,
            subject: subject.to_string(),
            subscriber,
        })
    }

    /// Number of connections attached **to this pod** (local registry only).
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.local.connection_count()
    }
}

impl Drop for NatsBackplane {
    fn drop(&mut self) {
        self.subscriber.abort();
    }
}

#[async_trait]
impl Backplane for NatsBackplane {
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
                if let Err(e) = self
                    .client
                    .publish(self.subject.clone(), payload.into())
                    .await
                {
                    tracing::warn!(error = %e, "backplane: nats publish failed; cross-pod delivery skipped");
                }
            }
            Err(e) => tracing::warn!(error = %e, "backplane: envelope serialize failed"),
        }
        n
    }
}
