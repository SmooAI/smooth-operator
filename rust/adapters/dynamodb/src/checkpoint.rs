//! DynamoDB-backed [`CheckpointStore`].
//!
//! smooth-operator ships `Memory`/`File`/`Sqlite`/`Postgres` checkpoint stores
//! but **no DynamoDB one** — this is it, for the AWS-serverless backend. Per
//! `docs/STORAGE.md`:
//!
//! - items live at `PK = CKPT#<agentId>`, `SK = <zero-padded iteration>#<id>`,
//! - `save` → `PutItem`,
//! - `load_latest` → `Query(Limit=1, ScanIndexForward=false)` (highest iteration),
//! - `load(id)` → `Query` on `GSI1 (CKPTID#<id>)`,
//! - `list` → `Query` (full partition, newest-first),
//! - `prune` → query all, delete all but the newest `keep` (batched).
//!
//! ## Sync trait over an async SDK
//!
//! [`CheckpointStore`](smooth_operator::CheckpointStore) is a **synchronous**
//! trait (the engine calls `save`/`load_latest` directly), but `aws-sdk-dynamodb`
//! is async. We bridge exactly like the Postgres adapter's knowledge base:
//! [`run_blocking`](DynamoCheckpointStore::run_blocking) `spawn`s the async work
//! onto a **captured runtime [`Handle`]** (so its I/O makes progress on that
//! runtime's reactor) and blocks the calling thread on the `JoinHandle` from a
//! throwaway OS thread that owns a tiny current-thread runtime — never calling
//! `Handle::block_on` on a runtime worker thread (which panics "Cannot start a
//! runtime from within a runtime").
//!
//! ## Large-item spill
//!
//! A checkpoint's serialized `Conversation` blob can exceed DynamoDB's 400 KB
//! item limit on long threads. `save` checks the encoded body size against
//! [`MAX_INLINE_BLOB`]; if exceeded it returns a clear error pointing at the
//! S3-spill follow-up rather than silently failing a `PutItem`. The S3 spill
//! itself is the documented TODO (see [`MAX_INLINE_BLOB`]).

use anyhow::{anyhow, Result};
use aws_sdk_dynamodb::types::{AttributeValue, WriteRequest};
use aws_sdk_dynamodb::Client;
use tokio::runtime::Handle;

use smooth_operator::{Checkpoint, CheckpointStore};

use crate::keys::{self, attr};

/// Conservative inline-blob ceiling. DynamoDB's hard item limit is 400 KB; we
/// trip earlier (~300 KB) to leave headroom for keys/metadata and to fail loud
/// *before* the SDK rejects the `PutItem`. Blobs above this must spill to S3.
///
/// TODO(S3-spill): write the serialized conversation to an S3 object and store
/// `{ "s3Pointer": "<bucket>/<key>" }` in place of the inline body, then fetch
/// it on load. Tracked as the large-item follow-up in `docs/STORAGE.md`.
pub const MAX_INLINE_BLOB: usize = 300 * 1024;

/// DynamoDB-backed checkpoint store. Cheap to clone (a `Client` + `Handle` +
/// table name behind `Arc`-able fields).
#[derive(Clone)]
pub struct DynamoCheckpointStore {
    client: Client,
    table: String,
    handle: Handle,
}

impl DynamoCheckpointStore {
    /// Build over an existing DynamoDB client + table, capturing the current
    /// Tokio runtime handle for the sync→async bridge.
    ///
    /// # Panics
    /// Panics if called outside a Tokio runtime (no current `Handle`). The
    /// adapter constructs this from inside its async `connect`, so this holds.
    #[must_use]
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
            handle: Handle::current(),
        }
    }

    /// Drive an async future to completion from a *synchronous* trait method.
    ///
    /// `CheckpointStore` is sync; the SDK is async. We `spawn` the work onto the
    /// captured runtime (so its async I/O makes progress on that runtime's
    /// reactor) and block on the `JoinHandle` from a throwaway OS thread running
    /// a tiny current-thread runtime. This never calls `Handle::block_on` on a
    /// runtime worker thread, so it is safe whether the caller is on a worker or
    /// a plain OS thread.
    fn run_blocking<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let join = self.handle.spawn(fut);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> Result<T> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                let joined = rt.block_on(join);
                joined.map_err(|e| anyhow!("checkpoint task panicked or was cancelled: {e}"))?
            })();
            let _ = tx.send(result);
        });
        rx.recv()
            .map_err(|e| anyhow!("checkpoint task channel closed: {e}"))?
    }

    async fn save_async(&self, checkpoint: Checkpoint) -> Result<()> {
        let body = serde_json::to_string(&checkpoint)?;
        if body.len() > MAX_INLINE_BLOB {
            return Err(anyhow!(
                "checkpoint body is {} bytes (> {} inline limit); S3 spill not yet wired \
                 (see DynamoCheckpointStore::MAX_INLINE_BLOB / docs/STORAGE.md large-item TODO)",
                body.len(),
                MAX_INLINE_BLOB
            ));
        }
        self.client
            .put_item()
            .table_name(&self.table)
            .item(
                attr::PK,
                AttributeValue::S(keys::ckpt_pk(&checkpoint.agent_id)),
            )
            .item(
                attr::SK,
                AttributeValue::S(keys::ckpt_sk(checkpoint.iteration, &checkpoint.id)),
            )
            .item(
                attr::GSI1PK,
                AttributeValue::S(keys::ckpt_id_gsi1pk(&checkpoint.id)),
            )
            .item(
                attr::GSI1SK,
                AttributeValue::S(keys::ckpt_sk(checkpoint.iteration, &checkpoint.id)),
            )
            .item(attr::ENTITY, AttributeValue::S("checkpoint".to_string()))
            .item(attr::BODY, AttributeValue::S(body))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb put checkpoint: {}", aws_err(e)))?;
        Ok(())
    }

    fn checkpoint_from_item(
        item: &std::collections::HashMap<String, AttributeValue>,
    ) -> Result<Checkpoint> {
        let body = item
            .get(attr::BODY)
            .and_then(|v| v.as_s().ok())
            .ok_or_else(|| anyhow!("checkpoint item missing '{}' attribute", attr::BODY))?;
        Ok(serde_json::from_str(body)?)
    }

    async fn load_latest_async(&self, agent_id: String) -> Result<Option<Checkpoint>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("#pk = :pk")
            .expression_attribute_names("#pk", attr::PK)
            .expression_attribute_values(":pk", AttributeValue::S(keys::ckpt_pk(&agent_id)))
            .scan_index_forward(false) // highest iteration first
            .limit(1)
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb query load_latest: {}", aws_err(e)))?;
        match out.items().first() {
            Some(item) => Ok(Some(Self::checkpoint_from_item(item)?)),
            None => Ok(None),
        }
    }

    async fn load_async(&self, checkpoint_id: String) -> Result<Option<Checkpoint>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .index_name(keys::GSI1)
            .key_condition_expression("#pk = :pk")
            .expression_attribute_names("#pk", attr::GSI1PK)
            .expression_attribute_values(
                ":pk",
                AttributeValue::S(keys::ckpt_id_gsi1pk(&checkpoint_id)),
            )
            .limit(1)
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb query load by id: {}", aws_err(e)))?;
        match out.items().first() {
            Some(item) => Ok(Some(Self::checkpoint_from_item(item)?)),
            None => Ok(None),
        }
    }

    /// Query the full partition for an agent, newest (highest iteration) first.
    async fn list_async(&self, agent_id: String) -> Result<Vec<Checkpoint>> {
        let mut checkpoints = Vec::new();
        let mut last_key: Option<std::collections::HashMap<String, AttributeValue>> = None;
        loop {
            let mut req = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("#pk = :pk")
                .expression_attribute_names("#pk", attr::PK)
                .expression_attribute_values(":pk", AttributeValue::S(keys::ckpt_pk(&agent_id)))
                .scan_index_forward(false);
            if let Some(start) = last_key.take() {
                req = req.set_exclusive_start_key(Some(start));
            }
            let out = req
                .send()
                .await
                .map_err(|e| anyhow!("dynamodb query list: {}", aws_err(e)))?;
            for item in out.items() {
                checkpoints.push(Self::checkpoint_from_item(item)?);
            }
            match out.last_evaluated_key() {
                Some(k) if !k.is_empty() => last_key = Some(k.clone()),
                _ => break,
            }
        }
        Ok(checkpoints)
    }

    /// Keep the newest `keep` checkpoints for `agent_id`, delete the rest. The
    /// list is already iteration-descending, so the tail past `keep` is the
    /// oldest. Deletes are batched (`BatchWriteItem`, 25 per request).
    async fn prune_async(&self, agent_id: String, keep: usize) -> Result<usize> {
        let all = self.list_async(agent_id.clone()).await?;
        if all.len() <= keep {
            return Ok(0);
        }
        let to_delete: Vec<&Checkpoint> = all.iter().skip(keep).collect();
        let count = to_delete.len();

        for chunk in to_delete.chunks(25) {
            let requests: Vec<WriteRequest> = chunk
                .iter()
                .map(|cp| {
                    let key = std::collections::HashMap::from([
                        (
                            attr::PK.to_string(),
                            AttributeValue::S(keys::ckpt_pk(&cp.agent_id)),
                        ),
                        (
                            attr::SK.to_string(),
                            AttributeValue::S(keys::ckpt_sk(cp.iteration, &cp.id)),
                        ),
                    ]);
                    WriteRequest::builder()
                        .delete_request(
                            aws_sdk_dynamodb::types::DeleteRequest::builder()
                                .set_key(Some(key))
                                .build()
                                .expect("delete request has key"),
                        )
                        .build()
                })
                .collect();

            let mut unprocessed = Some(std::collections::HashMap::from([(
                self.table.clone(),
                requests,
            )]));
            // Drain unprocessed items (DynamoDB can return them under load).
            while let Some(items) = unprocessed.take() {
                if items.values().all(Vec::is_empty) {
                    break;
                }
                let out = self
                    .client
                    .batch_write_item()
                    .set_request_items(Some(items))
                    .send()
                    .await
                    .map_err(|e| anyhow!("dynamodb batch delete: {}", aws_err(e)))?;
                let leftovers = out.unprocessed_items().cloned().unwrap_or_default();
                if leftovers.values().all(Vec::is_empty) {
                    break;
                }
                unprocessed = Some(leftovers);
            }
        }
        Ok(count)
    }
}

impl CheckpointStore for DynamoCheckpointStore {
    fn save(&self, checkpoint: &Checkpoint) -> Result<()> {
        let this = self.clone();
        let cp = checkpoint.clone();
        self.run_blocking(async move { this.save_async(cp).await })
    }

    fn load_latest(&self, agent_id: &str) -> Result<Option<Checkpoint>> {
        let this = self.clone();
        let agent_id = agent_id.to_string();
        self.run_blocking(async move { this.load_latest_async(agent_id).await })
    }

    fn load(&self, checkpoint_id: &str) -> Result<Option<Checkpoint>> {
        let this = self.clone();
        let id = checkpoint_id.to_string();
        self.run_blocking(async move { this.load_async(id).await })
    }

    fn list(&self, agent_id: &str) -> Result<Vec<Checkpoint>> {
        let this = self.clone();
        let agent_id = agent_id.to_string();
        self.run_blocking(async move { this.list_async(agent_id).await })
    }

    fn prune(&self, agent_id: &str, keep: usize) -> Result<usize> {
        let this = self.clone();
        let agent_id = agent_id.to_string();
        self.run_blocking(async move { this.prune_async(agent_id, keep).await })
    }
}

/// Render an AWS SDK error (including the service error detail) into a string.
pub(crate) fn aws_err<E: std::error::Error>(e: aws_sdk_dynamodb::error::SdkError<E>) -> String {
    use aws_sdk_dynamodb::error::SdkError;
    match &e {
        SdkError::ServiceError(se) => format!("{}", se.err()),
        other => format!("{other}"),
    }
}
