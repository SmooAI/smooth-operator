//! Postgres + pgvector [`StorageAdapter`] — the dogfood backend.
//!
//! This is the production Postgres implementation of the one storage seam (see
//! `docs/STORAGE.md`). It mirrors the smooai monorepo's schema so dogfooding is
//! a swap, not a rewrite:
//!
//! - **OLTP** (conversations / participants / messages / sessions): async CRUD
//!   over a [`deadpool_postgres`] pool, semantics matching the in-memory baseline
//!   (conversation idempotency, external-id participant resolve, cursor message
//!   paging, session status/counts).
//! - **Checkpoints**: smooth-operator's
//!   [`PostgresCheckpointStore`](smooth_operator_core::PostgresCheckpointStore) (a
//!   *synchronous* r2d2-pooled store) constructed against the **same database**
//!   — so the engine's `with_checkpoint_store` plugs straight in and agent state
//!   lives next to the conversations it belongs to.
//! - **Knowledge**: a pgvector-backed [`PgKnowledgeBase`] (dense HNSW cosine ∪
//!   sparse `tsvector` BM25 → Reciprocal Rank Fusion). Text→vector goes through
//!   the [`Embedder`] seam — [`DeterministicEmbedder`] by default (reproducible,
//!   no network), [`GatewayEmbedder`] when a live gateway is configured.
//! - **Memory**: a pgvector-backed [`PgMemory`] (parity gap Phase 3 /
//!   SMOODEV-1470) — persistent, semantic, cross-thread agent memory namespaced
//!   by `(organization_id, user_id)` like the TS `['memories', orgId, userId]`
//!   store. Implements the core [`Memory`](smooth_operator_core::Memory) trait;
//!   recall is pgvector cosine top-K under an HNSW index, scoped to the
//!   namespace. Shares the adapter's [`Embedder`].
//!
//! ## Sharing one database between the async pool and the sync checkpoint store
//!
//! The OLTP/knowledge slices need async (`tokio-postgres` + `deadpool`); the
//! checkpoint slice is a sync trait backed by an r2d2 pool inside
//! smooth-operator. Both are pointed at the **same `conn_str`**: we build the
//! async deadpool from it for our own queries, and hand the *same* string to
//! `PostgresCheckpointStore::connect`, which stands up its own small r2d2 pool
//! and `checkpoints` table in that database. Two pools, two driver stacks, one
//! Postgres — the tables coexist (ours from [`schema`], theirs from their own
//! `CREATE TABLE IF NOT EXISTS`).

mod admin;
mod embedder;
mod knowledge;
mod memory;
mod reranker;
mod schema;

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use deadpool_postgres::{Config as PoolConfig, ManagerConfig, Pool, RecyclingMethod, Runtime};
use tokio_postgres::NoTls;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{
    ConversationUpdate, MessagePage, MessageQuery, SessionUpdate, StorageAdapter,
};
use smooth_operator::domain::{
    Conversation, Direction, Message, MessageContent, Participant, ParticipantRef, ParticipantType,
    Platform, Session, SessionStatus,
};
use smooth_operator_core::checkpoint::PostgresCheckpointStore;
use smooth_operator_core::{CheckpointStore, KnowledgeBase};

// The shared embedding seam (trait + deterministic default) now lives in core;
// re-export it here so existing `postgres::{Embedder, DeterministicEmbedder, …}`
// consumers keep working. Only the adapter-specific `GatewayEmbedder` (+ its
// 1536-d constant) is defined locally.
pub use admin::{PgConnectorConfigStore, PgIndexingStore, PgSettingsStore};
pub use embedder::{GatewayEmbedder, OPENAI_SMALL_EMBEDDING_DIM};
pub use knowledge::PgKnowledgeBase;
pub use memory::PgMemory;
pub use reranker::{
    GatewayReranker, HttpRerankBackend, RerankBackend, RerankScore, DEFAULT_RERANK_MODEL,
};
pub use smooth_operator::embedding::{
    DeterministicEmbedder, Embedder, InputType, DEFAULT_EMBEDDING_DIM,
};

/// Postgres + pgvector storage adapter.
pub struct PostgresAdapter {
    pool: Pool,
    /// `Option` so [`Drop`] can `take()` the checkpoint store and dispose of it on
    /// a dedicated OS thread. The sync `postgres::Client`s inside its r2d2 pool
    /// run `block_on` in their own `Drop`, which panics on a Tokio worker thread
    /// ("Cannot start a runtime from within a runtime"). Disposing off-runtime
    /// keeps the adapter safe to drop from async code.
    checkpoints: Option<Arc<PostgresCheckpointStore>>,
    knowledge: Arc<PgKnowledgeBase>,
    /// Retained so the [`memory`](PostgresAdapter::memory) accessor can build
    /// namespace-bound [`PgMemory`] handles that embed identically to knowledge.
    embedder: Arc<dyn Embedder>,
    embedding_dim: usize,
    /// Captured runtime handle for the sync admin-store bridges (connector
    /// configs / settings / indexing runs over the same async pool).
    handle: tokio::runtime::Handle,
}

impl Drop for PostgresAdapter {
    fn drop(&mut self) {
        if let Some(checkpoints) = self.checkpoints.take() {
            // Move the (possibly last) strong ref off any Tokio worker thread so
            // the r2d2 pool's blocking `postgres::Client::drop` runs on a plain
            // OS thread where `block_on` is legal. Join it so disposal is
            // deterministic (and so a short-lived process actually closes its
            // connections).
            if let Ok(handle) = std::thread::Builder::new()
                .name("pg-checkpoint-drop".into())
                .spawn(move || drop(checkpoints))
            {
                let _ = handle.join();
            }
        }
    }
}

impl PostgresAdapter {
    /// Connect to Postgres, build the async pool + sync checkpoint store, and
    /// apply the schema. Uses the [`DeterministicEmbedder`] (1024-d) by default.
    ///
    /// `conn_str` is a libpq URL or `key=value` connection string; it is read
    /// from `DATABASE_URL` / `SMOOTH_AGENT_DATABASE_URL` by [`Self::from_env`].
    ///
    /// # Errors
    /// Returns an error if the connection string is invalid, either pool fails to
    /// build, or schema migration fails.
    pub async fn connect(conn_str: &str) -> Result<Self> {
        Self::connect_with_embedder(conn_str, Arc::new(DeterministicEmbedder::new())).await
    }

    /// As [`Self::connect`] but with a caller-supplied embedder. The adapter's
    /// vector column width is taken from `embedder.dim()`, so a 1536-d
    /// [`GatewayEmbedder`] and the `vector(1536)` column always agree.
    ///
    /// # Errors
    /// See [`Self::connect`].
    pub async fn connect_with_embedder(
        conn_str: &str,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Self> {
        let embedding_dim = embedder.dim();

        // --- async pool (OLTP + knowledge) ---
        let pg_config: tokio_postgres::Config = conn_str
            .parse()
            .context("parsing connection string for async pool")?;
        let mut cfg = PoolConfig::new();
        cfg.manager = Some(ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        });
        // deadpool builds its manager from a tokio_postgres::Config.
        cfg.dbname = pg_config.get_dbname().map(str::to_string);
        cfg.user = pg_config.get_user().map(str::to_string);
        cfg.password = pg_config
            .get_password()
            .map(|p| String::from_utf8_lossy(p).into_owned());
        if let Some(host) = pg_config.get_hosts().iter().find_map(|h| match h {
            tokio_postgres::config::Host::Tcp(t) => Some(t.clone()),
            tokio_postgres::config::Host::Unix(p) => p.to_str().map(str::to_string),
        }) {
            cfg.host = Some(host);
        }
        if let Some(port) = pg_config.get_ports().first().copied() {
            cfg.port = Some(port);
        }
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .context("building deadpool")?;

        // --- apply schema (OLTP unconditionally; pgvector knowledge table) ---
        {
            let client = pool
                .get()
                .await
                .context("acquiring connection for migration")?;
            client
                .batch_execute(schema::OLTP_SCHEMA)
                .await
                .context("applying OLTP schema")?;
            client
                .batch_execute(schema::ADMIN_SCHEMA)
                .await
                .context("applying admin schema")?;
            client
                .batch_execute(schema::VECTOR_EXTENSION)
                .await
                .context("creating pgvector extension")?;
            client
                .batch_execute(&schema::knowledge_vectors_schema(embedding_dim))
                .await
                .context("applying knowledge_vectors schema")?;
            client
                .batch_execute(&schema::memories_schema(embedding_dim))
                .await
                .context("applying memories schema")?;
        }

        // --- sync checkpoint store against the SAME database ---
        // PostgresCheckpointStore::connect runs blocking r2d2 setup; keep it off
        // the async worker threads.
        let cs_conn = conn_str.to_string();
        let checkpoints =
            tokio::task::spawn_blocking(move || PostgresCheckpointStore::connect(&cs_conn))
                .await
                .context("checkpoint store setup task panicked")?
                .context("constructing PostgresCheckpointStore")?;
        let checkpoints = Arc::new(checkpoints);

        // --- pgvector knowledge base (shares the async pool) ---
        let handle = tokio::runtime::Handle::current();
        let knowledge = Arc::new(PgKnowledgeBase::new(
            pool.clone(),
            embedder.clone(),
            handle.clone(),
            None,
        ));

        Ok(Self {
            pool,
            checkpoints: Some(checkpoints),
            knowledge,
            embedder,
            embedding_dim,
            handle,
        })
    }

    /// Connect using `DATABASE_URL` or `SMOOTH_AGENT_DATABASE_URL` (the latter
    /// wins if both are set).
    ///
    /// # Errors
    /// Returns an error if neither env var is set, or if [`Self::connect`] fails.
    pub async fn from_env() -> Result<Self> {
        let conn_str = std::env::var("SMOOTH_AGENT_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .map_err(|_| anyhow!("neither SMOOTH_AGENT_DATABASE_URL nor DATABASE_URL is set"))?;
        Self::connect(&conn_str).await
    }

    /// The embedding dimension this adapter's `knowledge_vectors` column uses.
    #[must_use]
    pub fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }

    /// A Postgres-backed [`ConnectorConfigStore`](smooth_operator::connector_config::ConnectorConfigStore)
    /// over this adapter's pool (the `connector_configs` table). Cheap to build
    /// (clones the pool handle); make as many as you like.
    #[must_use]
    pub fn connector_config_store(&self) -> PgConnectorConfigStore {
        PgConnectorConfigStore::new(self.pool.clone(), self.handle.clone())
    }

    /// A Postgres-backed [`SettingsStore`](smooth_operator::settings::SettingsStore)
    /// over this adapter's pool (the `agent_settings` table).
    #[must_use]
    pub fn settings_store(&self) -> PgSettingsStore {
        PgSettingsStore::new(self.pool.clone(), self.handle.clone())
    }

    /// A Postgres-backed [`IndexingStore`](smooth_operator_ingestion::indexing::IndexingStore)
    /// over this adapter's pool (the `indexing_runs` table).
    #[must_use]
    pub fn indexing_store(&self) -> PgIndexingStore {
        PgIndexingStore::new(self.pool.clone(), self.handle.clone())
    }

    /// A Postgres-backed [`Memory`](smooth_operator_core::Memory) over this
    /// adapter's pool (the `memories` table), bound to one `(organization_id,
    /// user_id)` namespace — persistent, semantic, cross-thread agent memory
    /// (parity gap Phase 3 / SMOODEV-1470). Pass `user_id = None` for org-wide
    /// memory. Embeds with the adapter's configured [`Embedder`] so memory and
    /// knowledge vectors share the same column width and hashing.
    ///
    /// Cheap to build (clones pool + embedder handles); make one per
    /// `(org, user)` you serve.
    #[must_use]
    pub fn memory(&self, organization_id: impl Into<String>, user_id: Option<String>) -> PgMemory {
        PgMemory::new(
            self.pool.clone(),
            self.embedder.clone(),
            self.handle.clone(),
            organization_id,
            user_id,
        )
    }
}

// --- row → domain helpers ---------------------------------------------------

fn platform_to_str(p: Platform) -> &'static str {
    match p {
        Platform::Web => "web",
        Platform::Messenger => "messenger",
        Platform::Instagram => "instagram",
        Platform::Email => "email",
        Platform::Discord => "discord",
        Platform::Phone => "phone",
        Platform::Sms => "sms",
        Platform::Slack => "slack",
        Platform::Whatsapp => "whatsapp",
        Platform::Tiktok => "tiktok",
    }
}

fn platform_from_str(s: &str) -> Result<Platform> {
    Ok(match s {
        "web" => Platform::Web,
        "messenger" => Platform::Messenger,
        "instagram" => Platform::Instagram,
        "email" => Platform::Email,
        "discord" => Platform::Discord,
        "phone" => Platform::Phone,
        "sms" => Platform::Sms,
        "slack" => Platform::Slack,
        "whatsapp" => Platform::Whatsapp,
        "tiktok" => Platform::Tiktok,
        other => return Err(anyhow!("unknown platform '{other}'")),
    })
}

fn participant_type_to_str(t: ParticipantType) -> &'static str {
    match t {
        ParticipantType::User => "user",
        ParticipantType::AiAgent => "ai-agent",
        ParticipantType::HumanAgent => "human-agent",
    }
}

fn participant_type_from_str(s: &str) -> Result<ParticipantType> {
    Ok(match s {
        "user" => ParticipantType::User,
        "ai-agent" => ParticipantType::AiAgent,
        "human-agent" => ParticipantType::HumanAgent,
        other => return Err(anyhow!("unknown participant type '{other}'")),
    })
}

fn direction_to_str(d: Direction) -> &'static str {
    match d {
        Direction::Inbound => "inbound",
        Direction::Outbound => "outbound",
    }
}

fn direction_from_str(s: &str) -> Result<Direction> {
    Ok(match s {
        "inbound" => Direction::Inbound,
        "outbound" => Direction::Outbound,
        other => return Err(anyhow!("unknown direction '{other}'")),
    })
}

fn session_status_to_str(s: SessionStatus) -> &'static str {
    match s {
        SessionStatus::Active => "active",
        SessionStatus::Idle => "idle",
        SessionStatus::Ended => "ended",
    }
}

fn session_status_from_str(s: &str) -> Result<SessionStatus> {
    Ok(match s {
        "active" => SessionStatus::Active,
        "idle" => SessionStatus::Idle,
        "ended" => SessionStatus::Ended,
        other => return Err(anyhow!("unknown session status '{other}'")),
    })
}

fn row_to_conversation(row: &tokio_postgres::Row) -> Result<Conversation> {
    Ok(Conversation {
        id: row.get("id"),
        platform: platform_from_str(row.get::<_, String>("platform").as_str())?,
        name: row.get("name"),
        organization_id: row.get("organization_id"),
        idempotency_key: row.get("idempotency_key"),
        metadata_json: row.get("metadata_json"),
        analytics_json: row.get("analytics_json"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn row_to_participant(row: &tokio_postgres::Row) -> Result<Participant> {
    Ok(Participant {
        id: row.get("id"),
        conversation_id: row.get("conversation_id"),
        organization_id: row.get("organization_id"),
        participant_type: participant_type_from_str(row.get::<_, String>("type").as_str())?,
        external_id: row.get("external_id"),
        internal_id: row.get("internal_id"),
        browser_fingerprint: row.get("browser_fingerprint"),
        browser_info: row.get("browser_info"),
        name: row.get("name"),
        email: row.get("email"),
        phone: row.get("phone"),
        crm_contact_id: row.get("crm_contact_id"),
        metadata_json: row.get("metadata_json"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn row_to_message(row: &tokio_postgres::Row) -> Result<Message> {
    let content: serde_json::Value = row.get("content");
    let content: MessageContent =
        serde_json::from_value(content).context("decoding message content")?;
    let from: Option<serde_json::Value> = row.get("from_ref");
    let to: Option<serde_json::Value> = row.get("to_ref");
    let from: Option<ParticipantRef> = from
        .map(serde_json::from_value)
        .transpose()
        .context("decoding from_ref")?;
    let to: Option<ParticipantRef> = to
        .map(serde_json::from_value)
        .transpose()
        .context("decoding to_ref")?;
    Ok(Message {
        id: row.get("id"),
        external_id: row.get("external_id"),
        organization_id: row.get("organization_id"),
        conversation_id: row.get("conversation_id"),
        direction: direction_from_str(row.get::<_, String>("direction").as_str())?,
        content,
        from,
        to,
        metadata_json: row.get("metadata_json"),
        analytics_json: row.get("analytics_json"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn row_to_session(row: &tokio_postgres::Row) -> Result<Session> {
    let status: Option<String> = row.get("status");
    let status = status.map(|s| session_status_from_str(&s)).transpose()?;
    let token_count: Option<i64> = row.get("token_count");
    let message_count: Option<i64> = row.get("message_count");
    let metadata: Option<serde_json::Value> = row.get("metadata");
    let metadata = metadata
        .map(serde_json::from_value)
        .transpose()
        .context("decoding session metadata")?;
    Ok(Session {
        session_id: row.get("session_id"),
        conversation_id: row.get("conversation_id"),
        agent_id: row.get("agent_id"),
        agent_name: row.get("agent_name"),
        user_participant_id: row.get("user_participant_id"),
        agent_participant_id: row.get("agent_participant_id"),
        thread_id: row.get("thread_id"),
        status,
        token_count: token_count.map(|v| u64::try_from(v).unwrap_or(0)),
        message_count: message_count.map(|v| u64::try_from(v).unwrap_or(0)),
        metadata,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        ended_at: row.get("ended_at"),
        last_activity_at: row.get("last_activity_at"),
    })
}

#[async_trait]
impl StorageAdapter for PostgresAdapter {
    // ---- conversations ---------------------------------------------------

    async fn create_conversation(&self, conversation: Conversation) -> Result<Conversation> {
        let client = self.pool.get().await?;
        // Idempotency on (org, idempotencyKey): INSERT, on conflict do nothing,
        // then read back whichever row owns the key (new or pre-existing).
        client
            .execute(
                "INSERT INTO conversations
                    (id, platform, name, organization_id, idempotency_key, metadata_json, analytics_json, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT (organization_id, idempotency_key) DO NOTHING",
                &[
                    &conversation.id,
                    &platform_to_str(conversation.platform),
                    &conversation.name,
                    &conversation.organization_id,
                    &conversation.idempotency_key,
                    &conversation.metadata_json,
                    &conversation.analytics_json,
                    &conversation.created_at,
                    &conversation.updated_at,
                ],
            )
            .await?;
        let row = client
            .query_one(
                "SELECT * FROM conversations WHERE organization_id = $1 AND idempotency_key = $2",
                &[&conversation.organization_id, &conversation.idempotency_key],
            )
            .await?;
        row_to_conversation(&row)
    }

    async fn get_conversation(&self, id: &str) -> Result<Option<Conversation>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt("SELECT * FROM conversations WHERE id = $1", &[&id])
            .await?;
        row.as_ref().map(row_to_conversation).transpose()
    }

    async fn list_conversations_by_org(&self, organization_id: &str) -> Result<Vec<Conversation>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT * FROM conversations WHERE organization_id = $1 ORDER BY created_at DESC",
                &[&organization_id],
            )
            .await?;
        rows.iter().map(row_to_conversation).collect()
    }

    async fn update_conversation(
        &self,
        id: &str,
        update: ConversationUpdate,
    ) -> Result<Conversation> {
        let client = self.pool.get().await?;
        let now = Utc::now();
        // COALESCE keeps existing values when the update field is NULL; the
        // metadata/analytics fields are explicitly settable (incl. clearing is
        // out of scope here, matching the in-memory "set only when Some").
        let set_metadata = update.metadata_json.is_some();
        let set_analytics = update.analytics_json.is_some();
        let row = client
            .query_one(
                "UPDATE conversations SET
                    name = COALESCE($2, name),
                    metadata_json = CASE WHEN $3 THEN $4 ELSE metadata_json END,
                    analytics_json = CASE WHEN $5 THEN $6 ELSE analytics_json END,
                    updated_at = $7
                 WHERE id = $1
                 RETURNING *",
                &[
                    &id,
                    &update.name,
                    &set_metadata,
                    &update.metadata_json,
                    &set_analytics,
                    &update.analytics_json,
                    &now,
                ],
            )
            .await
            .with_context(|| format!("conversation '{id}' not found"))?;
        row_to_conversation(&row)
    }

    // ---- participants ----------------------------------------------------

    async fn add_participant(&self, participant: Participant) -> Result<Participant> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO conversation_participants
                    (id, conversation_id, organization_id, type, external_id, internal_id,
                     browser_fingerprint, browser_info, name, email, phone, crm_contact_id,
                     metadata_json, created_at, updated_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
                &[
                    &participant.id,
                    &participant.conversation_id,
                    &participant.organization_id,
                    &participant_type_to_str(participant.participant_type),
                    &participant.external_id,
                    &participant.internal_id,
                    &participant.browser_fingerprint,
                    &participant.browser_info,
                    &participant.name,
                    &participant.email,
                    &participant.phone,
                    &participant.crm_contact_id,
                    &participant.metadata_json,
                    &participant.created_at,
                    &participant.updated_at,
                ],
            )
            .await?;
        Ok(participant)
    }

    async fn get_participant(&self, id: &str) -> Result<Option<Participant>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT * FROM conversation_participants WHERE id = $1",
                &[&id],
            )
            .await?;
        row.as_ref().map(row_to_participant).transpose()
    }

    async fn list_participants_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<Participant>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT * FROM conversation_participants WHERE conversation_id = $1 ORDER BY created_at, id",
                &[&conversation_id],
            )
            .await?;
        rows.iter().map(row_to_participant).collect()
    }

    async fn resolve_participant_by_external_id(
        &self,
        conversation_id: &str,
        external_id: &str,
    ) -> Result<Option<Participant>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT * FROM conversation_participants
                 WHERE conversation_id = $1 AND external_id = $2
                 ORDER BY created_at LIMIT 1",
                &[&conversation_id, &external_id],
            )
            .await?;
        row.as_ref().map(row_to_participant).transpose()
    }

    // ---- messages --------------------------------------------------------

    async fn append_message(&self, message: Message) -> Result<Message> {
        let client = self.pool.get().await?;
        let content = serde_json::to_value(&message.content)?;
        let from = message
            .from
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        let to = message.to.as_ref().map(serde_json::to_value).transpose()?;
        client
            .execute(
                "INSERT INTO conversation_messages
                    (id, external_id, organization_id, conversation_id, direction, content,
                     from_ref, to_ref, metadata_json, analytics_json, created_at, updated_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
                &[
                    &message.id,
                    &message.external_id,
                    &message.organization_id,
                    &message.conversation_id,
                    &direction_to_str(message.direction),
                    &content,
                    &from,
                    &to,
                    &message.metadata_json,
                    &message.analytics_json,
                    &message.created_at,
                    &message.updated_at,
                ],
            )
            .await?;
        Ok(message)
    }

    async fn get_message(&self, id: &str) -> Result<Option<Message>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt("SELECT * FROM conversation_messages WHERE id = $1", &[&id])
            .await?;
        row.as_ref().map(row_to_message).transpose()
    }

    async fn list_messages_by_conversation(&self, query: MessageQuery) -> Result<MessagePage> {
        let client = self.pool.get().await?;
        let limit_i64 = i64::try_from(query.limit).unwrap_or(i64::MAX);

        // Cursor is a message id; page starts strictly after that message's seq
        // (or before, when descending). Resolve it to a seq first.
        let cursor_seq: Option<i64> = match &query.cursor {
            Some(cursor) => {
                let row = client
                    .query_opt(
                        "SELECT seq FROM conversation_messages WHERE id = $1",
                        &[&cursor],
                    )
                    .await?;
                row.map(|r| r.get::<_, i64>("seq"))
            }
            None => None,
        };

        // Fetch limit + 1 to detect whether another page remains, mirroring the
        // in-memory "next_cursor is Some iff more rows follow" contract.
        let probe = limit_i64.saturating_add(1);
        let rows = if query.descending {
            // Newest first: seq descending; cursor means "seq < cursor_seq".
            match cursor_seq {
                Some(seq) => {
                    client
                        .query(
                            "SELECT * FROM conversation_messages
                             WHERE conversation_id = $1 AND seq < $2
                             ORDER BY seq DESC LIMIT $3",
                            &[&query.conversation_id, &seq, &probe],
                        )
                        .await?
                }
                None => {
                    client
                        .query(
                            "SELECT * FROM conversation_messages
                             WHERE conversation_id = $1
                             ORDER BY seq DESC LIMIT $2",
                            &[&query.conversation_id, &probe],
                        )
                        .await?
                }
            }
        } else {
            // Oldest first: seq ascending; cursor means "seq > cursor_seq".
            match cursor_seq {
                Some(seq) => {
                    client
                        .query(
                            "SELECT * FROM conversation_messages
                             WHERE conversation_id = $1 AND seq > $2
                             ORDER BY seq ASC LIMIT $3",
                            &[&query.conversation_id, &seq, &probe],
                        )
                        .await?
                }
                None => {
                    client
                        .query(
                            "SELECT * FROM conversation_messages
                             WHERE conversation_id = $1
                             ORDER BY seq ASC LIMIT $2",
                            &[&query.conversation_id, &probe],
                        )
                        .await?
                }
            }
        };

        let has_more = rows.len() as i64 > limit_i64;
        let page_rows = if has_more {
            &rows[..query.limit]
        } else {
            &rows[..]
        };
        let messages: Vec<Message> = page_rows
            .iter()
            .map(row_to_message)
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
        let client = self.pool.get().await?;
        let status = session.status.map(session_status_to_str);
        let token_count = session
            .token_count
            .map(|v| i64::try_from(v).unwrap_or(i64::MAX));
        let message_count = session
            .message_count
            .map(|v| i64::try_from(v).unwrap_or(i64::MAX));
        let metadata = session
            .metadata
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        client
            .execute(
                "INSERT INTO conversation_sessions
                    (session_id, conversation_id, agent_id, agent_name, user_participant_id,
                     agent_participant_id, thread_id, status, token_count, message_count,
                     metadata, created_at, updated_at, ended_at, last_activity_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
                &[
                    &session.session_id,
                    &session.conversation_id,
                    &session.agent_id,
                    &session.agent_name,
                    &session.user_participant_id,
                    &session.agent_participant_id,
                    &session.thread_id,
                    &status,
                    &token_count,
                    &message_count,
                    &metadata,
                    &session.created_at,
                    &session.updated_at,
                    &session.ended_at,
                    &session.last_activity_at,
                ],
            )
            .await?;
        Ok(session)
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<Session>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT * FROM conversation_sessions WHERE session_id = $1",
                &[&session_id],
            )
            .await?;
        row.as_ref().map(row_to_session).transpose()
    }

    async fn update_session(&self, session_id: &str, update: SessionUpdate) -> Result<Session> {
        let client = self.pool.get().await?;
        let now = Utc::now();
        let status = update.status.map(session_status_to_str);
        let token_count = update
            .token_count
            .map(|v| i64::try_from(v).unwrap_or(i64::MAX));
        let message_count = update
            .message_count
            .map(|v| i64::try_from(v).unwrap_or(i64::MAX));
        // last_activity_at / ended_at are set only when Some (mirrors in-memory).
        let set_last_activity = update.last_activity_at.is_some();
        let set_ended = update.ended_at.is_some();
        let row = client
            .query_one(
                "UPDATE conversation_sessions SET
                    status = COALESCE($2, status),
                    token_count = COALESCE($3, token_count),
                    message_count = COALESCE($4, message_count),
                    last_activity_at = CASE WHEN $5 THEN $6 ELSE last_activity_at END,
                    ended_at = CASE WHEN $7 THEN $8 ELSE ended_at END,
                    updated_at = $9
                 WHERE session_id = $1
                 RETURNING *",
                &[
                    &session_id,
                    &status,
                    &token_count,
                    &message_count,
                    &set_last_activity,
                    &update.last_activity_at,
                    &set_ended,
                    &update.ended_at,
                    &now,
                ],
            )
            .await
            .with_context(|| format!("session '{session_id}' not found"))?;
        row_to_session(&row)
    }

    async fn list_sessions_by_conversation(&self, conversation_id: &str) -> Result<Vec<Session>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT * FROM conversation_sessions WHERE conversation_id = $1 ORDER BY created_at, session_id",
                &[&conversation_id],
            )
            .await?;
        rows.iter().map(row_to_session).collect()
    }

    // ---- engine accessors ------------------------------------------------

    fn checkpoints(&self) -> Arc<dyn CheckpointStore> {
        // Always `Some` between construction and drop.
        self.checkpoints
            .as_ref()
            .expect("checkpoint store present")
            .clone()
    }

    fn knowledge(&self) -> Arc<dyn KnowledgeBase> {
        self.knowledge.clone()
    }

    fn knowledge_for_access(&self, access: &AccessContext) -> Arc<dyn KnowledgeBase> {
        // Durable document-level ACL (feature gap G3): the returned handle
        // filters every query by the requester's entitlements against the stored
        // `acl` column **in SQL**, so a restricted document is never fetched —
        // and the filter survives the ingest→serve process boundary (unlike the
        // in-memory side table). See `knowledge::PgKnowledgeBase::query_async`.
        Arc::new(self.knowledge.with_access(access.clone()))
    }
}
