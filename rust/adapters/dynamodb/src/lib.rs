//! DynamoDB single-table [`StorageAdapter`] — the AWS-serverless backend.
//!
//! This is the production DynamoDB implementation of the one storage seam (see
//! `docs/STORAGE.md`). One table, overloaded `PK`/`SK` + two GSIs serve every
//! access pattern (the keys live in [`keys`]):
//!
//! - **OLTP** (conversations / participants / messages / sessions): async CRUD
//!   over `aws-sdk-dynamodb`, with the **same observable semantics** as the
//!   in-memory / Postgres baselines — conversation idempotency on
//!   `(org, idempotencyKey)` via a conditional GSI item, external-id participant
//!   resolve via `GSI1`, monotonic message sequencing (an atomic counter item)
//!   with cursor paging, session status/counts.
//! - **Checkpoints**: [`DynamoCheckpointStore`] — smooth-operator ships no
//!   DynamoDB checkpoint store, so this crate provides one (sync
//!   [`CheckpointStore`](smooth_operator::CheckpointStore) bridged over the async
//!   SDK; see [`checkpoint`]).
//! - **Knowledge**: [`DynamoKnowledgeBase`] — brute-force cosine over DynamoDB
//!   by default (testable, no extra services), or Amazon S3 Vectors behind the
//!   `s3-vectors` feature (see [`knowledge`] / [`s3vectors`]).
//!
//! ## Single-table key design
//!
//! | Entity        | PK                  | SK                              | GSI1PK                        |
//! | ------------- | ------------------- | ------------------------------- | ----------------------------- |
//! | Conversation  | `ORG#<org>`         | `CONV#<convId>`                 | `IDEM#<org>#<key>` + `CONV#<id>` |
//! | Participant   | `CONV#<convId>`     | `PART#<partId>`                 | `EXTERNAL#<convId>#<extId>`   |
//! | Message       | `CONV#<convId>`     | `MSG#<padded seq>#<id>`         | `MSG#<id>`                    |
//! | Session       | `CONV#<convId>`     | `SESS#<sessionId>`              | `SESSION#<sessionId>`        |
//! | Checkpoint    | `CKPT#<agentId>`    | `<padded iteration>#<id>`       | `CKPTID#<id>`                |
//! | Knowledge     | `KNOW#<org>`        | `DOC#<docId>`                   | —                             |
//!
//! Each conversation writes **two** GSI1 entries (one by-idempotency, one
//! by-id), so both `create_conversation` idempotency lookups and standalone
//! `get_conversation(id)` are single queries.

mod checkpoint;
mod embedder;
mod keys;
mod knowledge;
#[cfg(feature = "s3-vectors")]
mod s3vectors;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, GlobalSecondaryIndex, KeySchemaElement,
    KeyType, Projection, ProjectionType, ScalarAttributeType,
};
use aws_sdk_dynamodb::Client;
use chrono::Utc;
use tokio::runtime::Handle;

use smooth_operator::{CheckpointStore, KnowledgeBase};
use smooth_operator_agent_core::adapter::{
    ConversationUpdate, MessagePage, MessageQuery, SessionUpdate, StorageAdapter,
};
use smooth_operator_agent_core::domain::{Conversation, Message, Participant, Session};

pub use checkpoint::{DynamoCheckpointStore, MAX_INLINE_BLOB};
pub use embedder::{DeterministicEmbedder, Embedder, InputType, DEFAULT_EMBEDDING_DIM};
pub use knowledge::{DynamoKnowledgeBase, KnowledgeBackend};
#[cfg(feature = "s3-vectors")]
pub use s3vectors::{S3VectorsConfig, S3VectorsStore};

use checkpoint::aws_err;
use keys::{attr, GSI1};

/// Default table name when none is configured.
pub const DEFAULT_TABLE_NAME: &str = "smooth-operator-agent";

/// DynamoDB single-table storage adapter.
pub struct DynamoDbAdapter {
    client: Client,
    table: String,
    checkpoints: Arc<DynamoCheckpointStore>,
    knowledge: Arc<DynamoKnowledgeBase>,
}

impl DynamoDbAdapter {
    /// Build the adapter over an existing client and table name. Captures the
    /// current Tokio runtime [`Handle`] for the sync checkpoint/knowledge
    /// bridges, so this must be called from within a Tokio runtime.
    ///
    /// The knowledge slice defaults to the brute-force DynamoDB backend with a
    /// [`DeterministicEmbedder`] and the org partition `"default"`. Use
    /// [`Self::with_knowledge`] to override.
    ///
    /// # Panics
    /// Panics if called outside a Tokio runtime.
    #[must_use]
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        let table = table.into();
        let handle = Handle::current();
        let checkpoints = Arc::new(DynamoCheckpointStore::new(client.clone(), table.clone()));
        let knowledge = Arc::new(DynamoKnowledgeBase::new(
            client.clone(),
            table.clone(),
            Arc::new(DeterministicEmbedder::new()),
            handle,
            "default",
            KnowledgeBackend::BruteForce,
        ));
        Self {
            client,
            table,
            checkpoints,
            knowledge,
        }
    }

    /// Override the knowledge slice (embedder, org partition, backend).
    #[must_use]
    pub fn with_knowledge(
        mut self,
        embedder: Arc<dyn Embedder>,
        organization_id: impl Into<String>,
        backend: KnowledgeBackend,
    ) -> Self {
        let handle = Handle::current();
        self.knowledge = Arc::new(DynamoKnowledgeBase::new(
            self.client.clone(),
            self.table.clone(),
            embedder,
            handle,
            organization_id,
            backend,
        ));
        self
    }

    /// Build from the ambient AWS config (env / profile / IMDS), using
    /// `SMOOTH_AGENT_DDB_TABLE` for the table name (falling back to
    /// [`DEFAULT_TABLE_NAME`]).
    ///
    /// `endpoint_url` (e.g. a DynamoDB-Local URL) overrides the resolved
    /// endpoint when `Some` — used by tests and local development.
    pub async fn from_env(endpoint_url: Option<&str>) -> Result<Self> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(url) = endpoint_url {
            loader = loader.endpoint_url(url);
        }
        let conf = loader.load().await;
        let client = Client::new(&conf);
        let table = std::env::var("SMOOTH_AGENT_DDB_TABLE")
            .unwrap_or_else(|_| DEFAULT_TABLE_NAME.to_string());
        Ok(Self::new(client, table))
    }

    /// The underlying DynamoDB client (e.g. for tests / advanced wiring).
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The table name this adapter reads/writes.
    #[must_use]
    pub fn table_name(&self) -> &str {
        &self.table
    }

    /// Create the single table with its overloaded primary key and `GSI1`, if it
    /// does not already exist. Idempotent — a `ResourceInUseException` (table
    /// already exists) is treated as success.
    ///
    /// On-demand billing (`PAY_PER_REQUEST`) so there's nothing to provision; the
    /// `gsi1` GSI projects all attributes so direct-lookup queries return full
    /// items.
    ///
    /// # Errors
    /// Returns an error if table creation fails for any reason other than the
    /// table already existing.
    pub async fn create_table(&self) -> Result<()> {
        let pk = |name: &str| {
            AttributeDefinition::builder()
                .attribute_name(name)
                .attribute_type(ScalarAttributeType::S)
                .build()
                .expect("attribute definition")
        };
        let key = |name: &str, kt: KeyType| {
            KeySchemaElement::builder()
                .attribute_name(name)
                .key_type(kt)
                .build()
                .expect("key schema element")
        };

        let gsi1 = GlobalSecondaryIndex::builder()
            .index_name(GSI1)
            .key_schema(key(attr::GSI1PK, KeyType::Hash))
            .key_schema(key(attr::GSI1SK, KeyType::Range))
            .projection(
                Projection::builder()
                    .projection_type(ProjectionType::All)
                    .build(),
            )
            .build()
            .map_err(|e| anyhow!("building gsi1: {e}"))?;

        let result = self
            .client
            .create_table()
            .table_name(&self.table)
            .billing_mode(BillingMode::PayPerRequest)
            .attribute_definitions(pk(attr::PK))
            .attribute_definitions(pk(attr::SK))
            .attribute_definitions(pk(attr::GSI1PK))
            .attribute_definitions(pk(attr::GSI1SK))
            .key_schema(key(attr::PK, KeyType::Hash))
            .key_schema(key(attr::SK, KeyType::Range))
            .global_secondary_indexes(gsi1)
            .send()
            .await;

        match result {
            Ok(_) => {}
            Err(e) => {
                // Table already exists → idempotent success.
                if let aws_sdk_dynamodb::error::SdkError::ServiceError(se) = &e {
                    if se.err().is_resource_in_use_exception() {
                        return Ok(());
                    }
                }
                return Err(anyhow!("create_table: {}", aws_err(e)));
            }
        }

        // Wait for the table to become ACTIVE so the first writes don't race
        // creation. Poll DescribeTable a bounded number of times.
        for _ in 0..60 {
            let desc = self
                .client
                .describe_table()
                .table_name(&self.table)
                .send()
                .await
                .map_err(|e| anyhow!("describe_table: {}", aws_err(e)))?;
            let active = desc
                .table()
                .and_then(|t| t.table_status())
                .is_some_and(|s| matches!(s, aws_sdk_dynamodb::types::TableStatus::Active));
            if active {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Err(anyhow!(
            "table '{}' did not become ACTIVE in time",
            self.table
        ))
    }
}

// --- item <-> domain helpers -----------------------------------------------

/// Decode a domain struct stored as a JSON string under [`attr::BODY`].
fn body_to<T: serde::de::DeserializeOwned>(item: &HashMap<String, AttributeValue>) -> Result<T> {
    let body = item
        .get(attr::BODY)
        .and_then(|v| v.as_s().ok())
        .ok_or_else(|| anyhow!("item missing '{}' attribute", attr::BODY))?;
    serde_json::from_str(body).context("decoding stored body")
}

#[async_trait]
impl StorageAdapter for DynamoDbAdapter {
    // ---- conversations ---------------------------------------------------

    async fn create_conversation(&self, conversation: Conversation) -> Result<Conversation> {
        // Idempotency: a conditional put on the by-idempotency GSI1 partition.
        // We model the idempotency claim as the conversation's own item carrying
        // both GSI1 entries; the conditional guards on the *idempotency* key so a
        // second create with the same (org, idempotencyKey) is rejected and we
        // read back the pre-existing row.
        let body = serde_json::to_string(&conversation)?;

        // Idempotency claim: a dedicated item keyed (PK=ORG#<org>, SK=IDEM#<key>)
        // guarded by `attribute_not_exists(sk)`. This is the *only* item whose
        // existence is conditioned on the idempotency key, so a second create
        // with the same (org, idempotencyKey) — even with a different conv id —
        // fails the condition and we read the pre-existing conversation back out
        // of the claim's stored body. The claim carries the full conversation
        // body so resolution needs no second read.
        let claim = self
            .client
            .put_item()
            .table_name(&self.table)
            .item(
                attr::PK,
                AttributeValue::S(keys::conv_pk(&conversation.organization_id)),
            )
            .item(
                attr::SK,
                AttributeValue::S(keys::conv_idem_sk(&conversation.idempotency_key)),
            )
            .item(
                attr::ENTITY,
                AttributeValue::S("conversation-idem".to_string()),
            )
            .item(
                "idempotencyKey",
                AttributeValue::S(conversation.idempotency_key.clone()),
            )
            .item(attr::BODY, AttributeValue::S(body.clone()))
            .condition_expression("attribute_not_exists(#sk)")
            .expression_attribute_names("#sk", attr::SK)
            .send()
            .await;

        if let Err(e) = claim {
            let is_conflict = matches!(
                &e,
                aws_sdk_dynamodb::error::SdkError::ServiceError(se)
                    if se.err().is_conditional_check_failed_exception()
            );
            if is_conflict {
                // The (org, idempotencyKey) is already claimed — return the
                // pre-existing conversation stored in the claim item.
                return self
                    .resolve_conversation_by_idempotency(
                        &conversation.organization_id,
                        &conversation.idempotency_key,
                    )
                    .await?
                    .ok_or_else(|| {
                        anyhow!("idempotency conflict but no existing conversation found")
                    });
            }
            return Err(anyhow!("dynamodb put idempotency claim: {}", aws_err(e)));
        }

        // Claim won — write the canonical (PK=ORG#<org>, SK=CONV#<id>) item with
        // the by-id GSI1 entry, plus the by-id pointer item.
        self.client
            .put_item()
            .table_name(&self.table)
            .item(
                attr::PK,
                AttributeValue::S(keys::conv_pk(&conversation.organization_id)),
            )
            .item(attr::SK, AttributeValue::S(keys::conv_sk(&conversation.id)))
            .item(
                attr::GSI1PK,
                AttributeValue::S(keys::conv_id_gsi1pk(&conversation.id)),
            )
            .item(
                attr::GSI1SK,
                AttributeValue::S(keys::conv_sk(&conversation.id)),
            )
            .item(attr::ENTITY, AttributeValue::S("conversation".to_string()))
            .item(
                "idempotencyKey",
                AttributeValue::S(conversation.idempotency_key.clone()),
            )
            .item(attr::BODY, AttributeValue::S(body.clone()))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb put conversation: {}", aws_err(e)))?;
        Ok(conversation)
    }

    async fn get_conversation(&self, id: &str) -> Result<Option<Conversation>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .index_name(GSI1)
            .key_condition_expression("#pk = :pk")
            .expression_attribute_names("#pk", attr::GSI1PK)
            .expression_attribute_values(":pk", AttributeValue::S(keys::conv_id_gsi1pk(id)))
            .limit(1)
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb get_conversation: {}", aws_err(e)))?;
        match out.items().first() {
            Some(item) => Ok(Some(body_to(item)?)),
            None => Ok(None),
        }
    }

    async fn list_conversations_by_org(&self, organization_id: &str) -> Result<Vec<Conversation>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("#pk = :pk AND begins_with(#sk, :skp)")
            .expression_attribute_names("#pk", attr::PK)
            .expression_attribute_names("#sk", attr::SK)
            .expression_attribute_values(":pk", AttributeValue::S(keys::conv_pk(organization_id)))
            .expression_attribute_values(":skp", AttributeValue::S("CONV#".to_string()))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb list_conversations_by_org: {}", aws_err(e)))?;
        let mut convs: Vec<Conversation> =
            out.items().iter().map(body_to).collect::<Result<_>>()?;
        // Newest first (matches the baseline contract).
        convs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(convs)
    }

    async fn update_conversation(
        &self,
        id: &str,
        update: ConversationUpdate,
    ) -> Result<Conversation> {
        let mut conv = self
            .get_conversation(id)
            .await?
            .ok_or_else(|| anyhow!("conversation '{id}' not found"))?;
        if let Some(name) = update.name {
            conv.name = name;
        }
        if update.metadata_json.is_some() {
            conv.metadata_json = update.metadata_json;
        }
        if update.analytics_json.is_some() {
            conv.analytics_json = update.analytics_json;
        }
        conv.updated_at = Utc::now();

        // Rewrite the canonical item (carries the by-id GSI1 entry) with the new
        // body. `attribute_exists(sk)` makes this an update, not an upsert.
        let body = serde_json::to_string(&conv)?;
        self.client
            .put_item()
            .table_name(&self.table)
            .item(
                attr::PK,
                AttributeValue::S(keys::conv_pk(&conv.organization_id)),
            )
            .item(attr::SK, AttributeValue::S(keys::conv_sk(&conv.id)))
            .item(
                attr::GSI1PK,
                AttributeValue::S(keys::conv_id_gsi1pk(&conv.id)),
            )
            .item(attr::GSI1SK, AttributeValue::S(keys::conv_sk(&conv.id)))
            .item(attr::ENTITY, AttributeValue::S("conversation".to_string()))
            .item(
                "idempotencyKey",
                AttributeValue::S(conv.idempotency_key.clone()),
            )
            .item(attr::BODY, AttributeValue::S(body.clone()))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb update_conversation: {}", aws_err(e)))?;
        // Keep the idempotency-claim body in sync so a later idempotent re-create
        // returns the updated conversation, not a stale snapshot.
        self.client
            .put_item()
            .table_name(&self.table)
            .item(
                attr::PK,
                AttributeValue::S(keys::conv_pk(&conv.organization_id)),
            )
            .item(
                attr::SK,
                AttributeValue::S(keys::conv_idem_sk(&conv.idempotency_key)),
            )
            .item(
                attr::ENTITY,
                AttributeValue::S("conversation-idem".to_string()),
            )
            .item(
                "idempotencyKey",
                AttributeValue::S(conv.idempotency_key.clone()),
            )
            .item(attr::BODY, AttributeValue::S(body))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb update_conversation idem sync: {}", aws_err(e)))?;
        Ok(conv)
    }

    // ---- participants ----------------------------------------------------

    async fn add_participant(&self, participant: Participant) -> Result<Participant> {
        let body = serde_json::to_string(&participant)?;
        let mut req = self
            .client
            .put_item()
            .table_name(&self.table)
            .item(
                attr::PK,
                AttributeValue::S(keys::part_pk(&participant.conversation_id)),
            )
            .item(attr::SK, AttributeValue::S(keys::part_sk(&participant.id)))
            .item(attr::ENTITY, AttributeValue::S("participant".to_string()))
            .item(attr::BODY, AttributeValue::S(body));
        // Only participants with an external id get a GSI1 resolve entry.
        if let Some(ext) = &participant.external_id {
            req = req
                .item(
                    attr::GSI1PK,
                    AttributeValue::S(keys::part_external_gsi1pk(
                        &participant.conversation_id,
                        ext,
                    )),
                )
                .item(
                    attr::GSI1SK,
                    AttributeValue::S(keys::part_sk(&participant.id)),
                );
        }
        req.send()
            .await
            .map_err(|e| anyhow!("dynamodb add_participant: {}", aws_err(e)))?;
        Ok(participant)
    }

    async fn get_participant(&self, id: &str) -> Result<Option<Participant>> {
        // No org/conv in hand; participants are keyed under their conversation,
        // so id-only lookup scans for the PART#<id> sort key. (A by-id GSI could
        // be added if this becomes hot; the baseline contract only needs it to
        // work.)
        let out = self
            .client
            .scan()
            .table_name(&self.table)
            .filter_expression("#sk = :sk")
            .expression_attribute_names("#sk", attr::SK)
            .expression_attribute_values(":sk", AttributeValue::S(keys::part_sk(id)))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb get_participant: {}", aws_err(e)))?;
        match out.items().first() {
            Some(item) => Ok(Some(body_to(item)?)),
            None => Ok(None),
        }
    }

    async fn list_participants_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<Participant>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("#pk = :pk AND begins_with(#sk, :skp)")
            .expression_attribute_names("#pk", attr::PK)
            .expression_attribute_names("#sk", attr::SK)
            .expression_attribute_values(":pk", AttributeValue::S(keys::part_pk(conversation_id)))
            .expression_attribute_values(
                ":skp",
                AttributeValue::S(keys::PART_SK_PREFIX.to_string()),
            )
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb list_participants: {}", aws_err(e)))?;
        let mut parts: Vec<Participant> = out.items().iter().map(body_to).collect::<Result<_>>()?;
        parts.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(parts)
    }

    async fn resolve_participant_by_external_id(
        &self,
        conversation_id: &str,
        external_id: &str,
    ) -> Result<Option<Participant>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .index_name(GSI1)
            .key_condition_expression("#pk = :pk")
            .expression_attribute_names("#pk", attr::GSI1PK)
            .expression_attribute_values(
                ":pk",
                AttributeValue::S(keys::part_external_gsi1pk(conversation_id, external_id)),
            )
            .limit(1)
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb resolve_participant: {}", aws_err(e)))?;
        match out.items().first() {
            Some(item) => Ok(Some(body_to(item)?)),
            None => Ok(None),
        }
    }

    // ---- messages --------------------------------------------------------

    async fn append_message(&self, message: Message) -> Result<Message> {
        let conv_id = message
            .conversation_id
            .clone()
            .ok_or_else(|| anyhow!("message has no conversation_id"))?;

        // Atomically hand out the next monotonic sequence for this conversation
        // (ADD seq :1 on a per-conversation counter item, returning the new value).
        let seq = self.next_message_seq(&conv_id).await?;

        let body = serde_json::to_string(&message)?;
        self.client
            .put_item()
            .table_name(&self.table)
            .item(attr::PK, AttributeValue::S(keys::msg_pk(&conv_id)))
            .item(attr::SK, AttributeValue::S(keys::msg_sk(seq, &message.id)))
            .item(
                attr::GSI1PK,
                AttributeValue::S(keys::msg_id_gsi1pk(&message.id)),
            )
            .item(
                attr::GSI1SK,
                AttributeValue::S(keys::msg_sk(seq, &message.id)),
            )
            .item(attr::ENTITY, AttributeValue::S("message".to_string()))
            .item(attr::SEQ, AttributeValue::N(seq.to_string()))
            .item(attr::BODY, AttributeValue::S(body))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb append_message: {}", aws_err(e)))?;
        Ok(message)
    }

    async fn get_message(&self, id: &str) -> Result<Option<Message>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .index_name(GSI1)
            .key_condition_expression("#pk = :pk")
            .expression_attribute_names("#pk", attr::GSI1PK)
            .expression_attribute_values(":pk", AttributeValue::S(keys::msg_id_gsi1pk(id)))
            .limit(1)
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb get_message: {}", aws_err(e)))?;
        match out.items().first() {
            Some(item) => Ok(Some(body_to(item)?)),
            None => Ok(None),
        }
    }

    async fn list_messages_by_conversation(&self, query: MessageQuery) -> Result<MessagePage> {
        // Resolve the cursor (a message id) to its SK so we can page strictly
        // after (ascending) / before (descending) it.
        let cursor_sk: Option<String> = match &query.cursor {
            Some(cursor) => self
                .message_sk_for_id(cursor)
                .await?
                .ok_or_else(|| anyhow!("cursor message '{cursor}' not found"))
                .map(Some)?,
            None => None,
        };

        let probe = i32::try_from(query.limit.saturating_add(1)).unwrap_or(i32::MAX);
        let mut req = self
            .client
            .query()
            .table_name(&self.table)
            .expression_attribute_names("#pk", attr::PK)
            .expression_attribute_names("#sk", attr::SK)
            .expression_attribute_values(
                ":pk",
                AttributeValue::S(keys::msg_pk(&query.conversation_id)),
            )
            .scan_index_forward(!query.descending)
            .limit(probe);

        // The conversation partition also holds non-message items (session,
        // participants, the seq counter), so every branch must stay inside the
        // `MSG#`…`MSG$` band. `MSG$` (`$` = 0x24, one past `#` = 0x23) is the
        // exclusive upper sentinel for the prefix.
        let msg_lo = keys::MSG_SK_PREFIX.to_string(); // "MSG#"
        let msg_hi = "MSG$".to_string();
        req = match (&cursor_sk, query.descending) {
            // Ascending, strictly after the cursor, capped to the MSG band.
            (Some(sk), false) => req
                .key_condition_expression("#pk = :pk AND #sk BETWEEN :lo AND :hi")
                .expression_attribute_values(
                    ":lo",
                    AttributeValue::S(format!("{sk}\u{0}")), // > cursor (next byte)
                )
                .expression_attribute_values(":hi", AttributeValue::S(msg_hi)),
            // Descending, strictly before the cursor, floored to the MSG band.
            (Some(sk), true) => req
                .key_condition_expression("#pk = :pk AND #sk BETWEEN :lo AND :hi")
                .expression_attribute_values(":lo", AttributeValue::S(msg_lo))
                // Trim the last char's successor so the cursor itself is excluded:
                // use the cursor SK as the inclusive top then drop it post-query.
                .expression_attribute_values(":hi", AttributeValue::S(sk.clone())),
            // No cursor: the whole MSG band.
            (None, _) => req
                .key_condition_expression("#pk = :pk AND begins_with(#sk, :skp)")
                .expression_attribute_values(":skp", AttributeValue::S(msg_lo)),
        };
        // For the descending+cursor case BETWEEN is inclusive of the cursor SK,
        // so we filter the cursor message out of the results below.
        let exclude_sk = if query.descending {
            cursor_sk.clone()
        } else {
            None
        };

        let out = req
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb list_messages: {}", aws_err(e)))?;

        // Drop the cursor item itself in the descending+cursor case (BETWEEN is
        // inclusive of `:hi` = the cursor SK).
        let items: Vec<&std::collections::HashMap<String, AttributeValue>> = out
            .items()
            .iter()
            .filter(|item| {
                exclude_sk.as_deref().is_none_or(|ex| {
                    item.get(attr::SK)
                        .and_then(|v| v.as_s().ok())
                        .map(String::as_str)
                        != Some(ex)
                })
            })
            .collect();

        let has_more = items.len() > query.limit;
        let page_items = if has_more {
            &items[..query.limit]
        } else {
            &items[..]
        };
        let messages: Vec<Message> = page_items
            .iter()
            .map(|item| body_to(item))
            .collect::<Result<_>>()?;
        let next_cursor = if has_more {
            messages.last().map(|m| m.id.clone())
        } else {
            None
        };
        Ok(MessagePage {
            messages,
            next_cursor,
        })
    }

    // ---- sessions --------------------------------------------------------

    async fn create_session(&self, session: Session) -> Result<Session> {
        let body = serde_json::to_string(&session)?;
        self.client
            .put_item()
            .table_name(&self.table)
            .item(
                attr::PK,
                AttributeValue::S(keys::sess_pk(&session.conversation_id)),
            )
            .item(
                attr::SK,
                AttributeValue::S(keys::sess_sk(&session.session_id)),
            )
            .item(
                attr::GSI1PK,
                AttributeValue::S(keys::sess_gsi1pk(&session.session_id)),
            )
            .item(
                attr::GSI1SK,
                AttributeValue::S(keys::sess_sk(&session.session_id)),
            )
            .item(attr::ENTITY, AttributeValue::S("session".to_string()))
            .item(attr::BODY, AttributeValue::S(body))
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb create_session: {}", aws_err(e)))?;
        Ok(session)
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<Session>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .index_name(GSI1)
            .key_condition_expression("#pk = :pk")
            .expression_attribute_names("#pk", attr::GSI1PK)
            .expression_attribute_values(":pk", AttributeValue::S(keys::sess_gsi1pk(session_id)))
            .limit(1)
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb get_session: {}", aws_err(e)))?;
        match out.items().first() {
            Some(item) => Ok(Some(body_to(item)?)),
            None => Ok(None),
        }
    }

    async fn update_session(&self, session_id: &str, update: SessionUpdate) -> Result<Session> {
        let mut session = self
            .get_session(session_id)
            .await?
            .ok_or_else(|| anyhow!("session '{session_id}' not found"))?;
        if let Some(status) = update.status {
            session.status = Some(status);
        }
        if let Some(token_count) = update.token_count {
            session.token_count = Some(token_count);
        }
        if let Some(message_count) = update.message_count {
            session.message_count = Some(message_count);
        }
        if update.last_activity_at.is_some() {
            session.last_activity_at = update.last_activity_at;
        }
        if update.ended_at.is_some() {
            session.ended_at = update.ended_at;
        }
        session.updated_at = Some(Utc::now());
        // Rewrite the canonical item (same keys) with the updated body.
        self.create_session(session.clone()).await?;
        Ok(session)
    }

    async fn list_sessions_by_conversation(&self, conversation_id: &str) -> Result<Vec<Session>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("#pk = :pk AND begins_with(#sk, :skp)")
            .expression_attribute_names("#pk", attr::PK)
            .expression_attribute_names("#sk", attr::SK)
            .expression_attribute_values(":pk", AttributeValue::S(keys::sess_pk(conversation_id)))
            .expression_attribute_values(
                ":skp",
                AttributeValue::S(keys::SESS_SK_PREFIX.to_string()),
            )
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb list_sessions: {}", aws_err(e)))?;
        let mut sessions: Vec<Session> = out.items().iter().map(body_to).collect::<Result<_>>()?;
        sessions.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(sessions)
    }

    // ---- engine accessors ------------------------------------------------

    fn checkpoints(&self) -> Arc<dyn CheckpointStore> {
        self.checkpoints.clone()
    }

    fn knowledge(&self) -> Arc<dyn KnowledgeBase> {
        self.knowledge.clone()
    }
}

impl DynamoDbAdapter {
    /// Resolve the conversation owning `(org, idempotencyKey)` by reading the
    /// idempotency-claim item directly (`GetItem` on PK=ORG#<org>, SK=IDEM#<key>).
    async fn resolve_conversation_by_idempotency(
        &self,
        org: &str,
        idempotency_key: &str,
    ) -> Result<Option<Conversation>> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key(attr::PK, AttributeValue::S(keys::conv_pk(org)))
            .key(
                attr::SK,
                AttributeValue::S(keys::conv_idem_sk(idempotency_key)),
            )
            .send()
            .await
            .map_err(|e| {
                anyhow!(
                    "dynamodb resolve_conversation_by_idempotency: {}",
                    aws_err(e)
                )
            })?;
        match out.item() {
            Some(item) => Ok(Some(body_to(item)?)),
            None => Ok(None),
        }
    }

    /// Atomically increment and return the next message sequence for a
    /// conversation (`UpdateItem ADD seq :1` with `RETURN_VALUES = UPDATED_NEW`).
    async fn next_message_seq(&self, conversation_id: &str) -> Result<u64> {
        let out = self
            .client
            .update_item()
            .table_name(&self.table)
            .key(
                attr::PK,
                AttributeValue::S(keys::seq_counter_pk(conversation_id)),
            )
            .key(
                attr::SK,
                AttributeValue::S(keys::SEQ_COUNTER_SK.to_string()),
            )
            .update_expression("ADD #seq :one")
            .expression_attribute_names("#seq", attr::SEQ)
            .expression_attribute_values(":one", AttributeValue::N("1".to_string()))
            .return_values(aws_sdk_dynamodb::types::ReturnValue::UpdatedNew)
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb next_message_seq: {}", aws_err(e)))?;
        let seq = out
            .attributes()
            .and_then(|a| a.get(attr::SEQ))
            .and_then(|v| v.as_n().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| anyhow!("seq counter returned no value"))?;
        Ok(seq)
    }

    /// Find the canonical SK of a message by its id (via the by-id GSI), so the
    /// paging cursor can be turned into an SK boundary.
    async fn message_sk_for_id(&self, msg_id: &str) -> Result<Option<String>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .index_name(GSI1)
            .key_condition_expression("#pk = :pk")
            .expression_attribute_names("#pk", attr::GSI1PK)
            .expression_attribute_values(":pk", AttributeValue::S(keys::msg_id_gsi1pk(msg_id)))
            .limit(1)
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb message_sk_for_id: {}", aws_err(e)))?;
        Ok(out
            .items()
            .first()
            .and_then(|item| item.get(attr::SK))
            .and_then(|v| v.as_s().ok())
            .cloned())
    }
}
