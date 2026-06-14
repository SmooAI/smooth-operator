//! Embedded schema / migration SQL, applied on [`PostgresAdapter::init`].
//!
//! Mirrors the smooai monorepo's relational shape so dogfooding is a swap, not a
//! rewrite: `conversations`, `conversation_participants`, `conversation_messages`,
//! `conversation_sessions`, plus `knowledge_vectors` (pgvector `embedding` +
//! generated `content_tsv` + HNSW cosine index).
//!
//! The `checkpoints` table is **not** created here — that is owned by
//! smooth-operator's [`PostgresCheckpointStore`](smooth_operator_core::PostgresCheckpointStore),
//! which runs its own `CREATE TABLE IF NOT EXISTS checkpoints …` against the same
//! database when the adapter constructs it. Keeping the DDL in its source crate
//! avoids two definitions of the same table drifting apart.

/// The OLTP tables (conversations / participants / messages / sessions). These
/// have no dependency on the pgvector extension, so they apply unconditionally.
pub const OLTP_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (
    id              TEXT PRIMARY KEY,
    platform        TEXT NOT NULL,
    name            TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    metadata_json   JSONB,
    analytics_json  JSONB,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL
);
-- Enforces conversation create idempotency on (org, idempotencyKey).
CREATE UNIQUE INDEX IF NOT EXISTS uq_conversations_org_idem
    ON conversations (organization_id, idempotency_key);
CREATE INDEX IF NOT EXISTS idx_conversations_org_created
    ON conversations (organization_id, created_at DESC);

CREATE TABLE IF NOT EXISTS conversation_participants (
    id                  TEXT PRIMARY KEY,
    conversation_id     TEXT NOT NULL,
    organization_id     TEXT NOT NULL,
    type                TEXT NOT NULL CHECK (type IN ('user', 'ai-agent', 'human-agent')),
    external_id         TEXT,
    internal_id         TEXT,
    browser_fingerprint TEXT,
    browser_info        JSONB,
    name                TEXT NOT NULL,
    email               TEXT,
    phone               TEXT,
    crm_contact_id      TEXT,
    metadata_json       JSONB,
    created_at          TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_participants_conversation
    ON conversation_participants (conversation_id, created_at);
-- Resolve a returning user by external identity within a conversation.
CREATE INDEX IF NOT EXISTS idx_participants_external
    ON conversation_participants (conversation_id, external_id);

CREATE TABLE IF NOT EXISTS conversation_messages (
    id              TEXT PRIMARY KEY,
    external_id     TEXT,
    organization_id TEXT,
    conversation_id TEXT,
    direction       TEXT NOT NULL CHECK (direction IN ('inbound', 'outbound')),
    content         JSONB NOT NULL,
    from_ref        JSONB,
    to_ref          JSONB,
    metadata_json   JSONB,
    analytics_json  JSONB,
    -- Monotonic append sequence per conversation; the stable paging cursor.
    seq             BIGSERIAL,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation_seq
    ON conversation_messages (conversation_id, seq);

CREATE TABLE IF NOT EXISTS conversation_sessions (
    session_id           TEXT PRIMARY KEY,
    conversation_id      TEXT NOT NULL,
    agent_id             TEXT NOT NULL,
    agent_name           TEXT NOT NULL,
    user_participant_id  TEXT NOT NULL,
    agent_participant_id TEXT NOT NULL,
    thread_id            TEXT NOT NULL,
    status               TEXT,
    token_count          BIGINT,
    message_count        BIGINT,
    metadata             JSONB,
    created_at           TIMESTAMPTZ,
    updated_at           TIMESTAMPTZ,
    ended_at             TIMESTAMPTZ,
    last_activity_at     TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_sessions_conversation
    ON conversation_sessions (conversation_id, created_at);
"#;

/// The admin-store tables (Phase 12 follow-up): the three management-console
/// stores made durable — connector configs, per-org agent settings, and the
/// indexing-run ledger. These have no dependency on pgvector, so they apply
/// unconditionally alongside the OLTP schema.
///
/// - `connector_configs` — PK `(org_id, id)`, org-scoped CRUD. `upsert` is an
///   `INSERT … ON CONFLICT (org_id, id) DO UPDATE`, `list` filters on `org_id`.
/// - `agent_settings` — PK `org_id`, one row per org; `put` is an upsert, `get`
///   falls back to defaults in the adapter when absent.
/// - `indexing_runs` — PK `id`, indexed `(connector_name, started_at DESC)` so
///   `list_runs` is an ordered scan and `latest_cursor` is a `max(cursor)` over
///   `status = 'succeeded'` rows only (a failed run never advances the cursor).
pub const ADMIN_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS connector_configs (
    org_id     TEXT NOT NULL,
    id         TEXT NOT NULL,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    config     JSONB NOT NULL,
    enabled    BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, id)
);

CREATE TABLE IF NOT EXISTS agent_settings (
    org_id        TEXT PRIMARY KEY,
    model         TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    default_tools JSONB NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS indexing_runs (
    id               TEXT PRIMARY KEY,
    connector_name   TEXT NOT NULL,
    status           TEXT NOT NULL,
    started_at       TIMESTAMPTZ NOT NULL,
    finished_at      TIMESTAMPTZ,
    documents_seen   BIGINT NOT NULL,
    chunks_indexed   BIGINT NOT NULL,
    documents_skipped BIGINT NOT NULL,
    cursor           TIMESTAMPTZ,
    error            TEXT
);
-- list_runs(name) orders by started_at; latest_cursor scans the succeeded rows.
CREATE INDEX IF NOT EXISTS idx_indexing_runs_connector_started
    ON indexing_runs (connector_name, started_at DESC);
"#;

/// pgvector extension. Requires a pgvector-enabled image
/// (`pgvector/pgvector:pg16` or `ankane/pgvector`).
pub const VECTOR_EXTENSION: &str = "CREATE EXTENSION IF NOT EXISTS vector;";

/// Build the `knowledge_vectors` DDL for a given embedding dimension.
///
/// Mirrors smooai's `knowledge_vectors`: an `embedding vector(N)` (Voyage-style,
/// default N=1024) for dense retrieval, a generated `content_tsv tsvector` for
/// BM25-style sparse retrieval, `metadata jsonb`, `organization_id`, and an HNSW
/// cosine index on the embedding. The dimension is parameterized so the column
/// width always matches the configured
/// [`Embedder`](smooth_operator::embedding::Embedder).
#[must_use]
pub fn knowledge_vectors_schema(dim: usize) -> String {
    format!(
        r#"
CREATE TABLE IF NOT EXISTS knowledge_vectors (
    id              TEXT PRIMARY KEY,
    document_id     TEXT NOT NULL,
    organization_id TEXT,
    source          TEXT NOT NULL,
    content         TEXT NOT NULL,
    embedding       vector({dim}) NOT NULL,
    content_tsv     tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
    metadata        JSONB,
    -- Document-level access control (feature gap G3), persisted so the ACL
    -- survives the ingest(process)→serve(process) boundary (the in-memory ACL
    -- side table cannot). The serialized DocAcl (`{{public, users[], groups[]}}`)
    -- the document carried at ingest; NULL ⇒ no ACL recorded ⇒ org-public
    -- (backward-compatible default). The chat retrieval path filters rows by the
    -- requester's entitlements against this column (see knowledge.rs query_async).
    acl             JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Idempotent add for tables created before the ACL column existed (so an
-- upgrade-in-place gets the column without a destructive migration).
ALTER TABLE knowledge_vectors ADD COLUMN IF NOT EXISTS acl JSONB;
-- Dense ANN: HNSW over cosine distance (the `<=>` operator).
CREATE INDEX IF NOT EXISTS idx_knowledge_embedding_hnsw
    ON knowledge_vectors USING hnsw (embedding vector_cosine_ops);
-- Sparse BM25-style keyword retrieval over the generated tsvector.
CREATE INDEX IF NOT EXISTS idx_knowledge_content_tsv
    ON knowledge_vectors USING gin (content_tsv);
CREATE INDEX IF NOT EXISTS idx_knowledge_org
    ON knowledge_vectors (organization_id);
"#
    )
}

/// Build the `memories` DDL for a given embedding dimension.
///
/// Cross-thread, semantic agent memory (parity gap Phase 3 / SMOODEV-1470).
/// Mirrors the TS side's Postgres `store`/`store_vectors` namespaced by
/// `['memories', orgId, userId]`: every row carries `(organization_id, user_id)`
/// so a [`PgMemory`](crate::PgMemory) instance bound to one namespace can never
/// recall another's rows. `user_id` is **nullable** for org-wide memory.
///
/// An `embedding vector(N)` (matching the active [`Embedder`] dim, default
/// N=1024 / Voyage-shaped) backs semantic recall via the pgvector cosine `<=>`
/// operator under an HNSW index, exactly like `knowledge_vectors`. The
/// `memory_type` mirrors the core [`MemoryType`](smooth_operator_core::MemoryType)
/// enum (serialized as its serde tag), `metadata` is the entry's JSON blob, and
/// `relevance` is persisted as written (recall overwrites it with the computed
/// cosine score on read).
#[must_use]
pub fn memories_schema(dim: usize) -> String {
    format!(
        r#"
CREATE TABLE IF NOT EXISTS memories (
    id              TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL,
    -- NULL ⇒ org-wide memory (not bound to a single user).
    user_id         TEXT,
    content         TEXT NOT NULL,
    memory_type     TEXT NOT NULL,
    relevance       REAL NOT NULL DEFAULT 0,
    metadata        JSONB NOT NULL DEFAULT '{{}}'::jsonb,
    embedding       vector({dim}) NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_accessed   TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Dense ANN: HNSW over cosine distance (the `<=>` operator), as knowledge_vectors.
CREATE INDEX IF NOT EXISTS idx_memories_embedding_hnsw
    ON memories USING hnsw (embedding vector_cosine_ops);
-- Namespace scan: recall always filters on (org_id, user_id) before ANN ranking.
CREATE INDEX IF NOT EXISTS idx_memories_namespace
    ON memories (organization_id, user_id);
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GatewayEmbedder, OPENAI_SMALL_EMBEDDING_DIM};
    use smooth_operator::embedding::{DeterministicEmbedder, Embedder, DEFAULT_EMBEDDING_DIM};

    /// The store's `vector(N)` column width MUST come from the active embedder's
    /// `dim()` — never a hardcoded constant. This is the silent-retrieval-breaker
    /// the adversarial review flagged: a 1536-d GatewayEmbedder writing into a
    /// 1024-d column (or vice versa) produces garbage retrieval with no error.
    ///
    /// Drive both embedders' real `dim()` through the schema builder (the same
    /// path `connect_with_embedder` takes at line `knowledge_vectors_schema(embedder.dim())`)
    /// and assert the column matches. No live DB needed — this is the wiring contract.
    #[test]
    fn store_column_width_matches_active_embedder_dim() {
        // Deterministic (offline / tests): 1024-d → vector(1024).
        let det = DeterministicEmbedder::new();
        assert_eq!(det.dim(), DEFAULT_EMBEDDING_DIM);
        let det_ddl = knowledge_vectors_schema(det.dim());
        assert!(
            det_ddl.contains("vector(1024)"),
            "deterministic embedder ({}-d) must yield a vector(1024) column, got:\n{det_ddl}",
            det.dim()
        );
        assert!(!det_ddl.contains("vector(1536)"));

        // Gateway (production): 1536-d → vector(1536). Built without a network
        // call — `dim()` is local config.
        let gw = GatewayEmbedder::new(
            "https://example.test/v1",
            "sk-test",
            "text-embedding-3-small",
            OPENAI_SMALL_EMBEDDING_DIM,
        );
        assert_eq!(gw.dim(), OPENAI_SMALL_EMBEDDING_DIM);
        let gw_ddl = knowledge_vectors_schema(gw.dim());
        assert!(
            gw_ddl.contains("vector(1536)"),
            "gateway embedder ({}-d) must yield a vector(1536) column, got:\n{gw_ddl}",
            gw.dim()
        );
        assert!(!gw_ddl.contains("vector(1024)"));
    }

    /// The `memories` column width tracks the active embedder's `dim()` for the
    /// same reason `knowledge_vectors` does — a dimension mismatch silently
    /// breaks cosine recall. Drive both embedders' real `dim()` through the
    /// schema builder (the path `PgMemory` takes at `memories_schema(dim)`).
    #[test]
    fn memories_column_width_matches_active_embedder_dim() {
        let det = DeterministicEmbedder::new();
        assert_eq!(det.dim(), DEFAULT_EMBEDDING_DIM);
        let det_ddl = memories_schema(det.dim());
        assert!(
            det_ddl.contains("vector(1024)"),
            "deterministic embedder ({}-d) must yield a vector(1024) column, got:\n{det_ddl}",
            det.dim()
        );
        assert!(!det_ddl.contains("vector(1536)"));
        // user_id must be nullable (org-wide memory) and the namespace must be indexed.
        assert!(det_ddl.contains("user_id         TEXT,"));
        assert!(det_ddl.contains("idx_memories_namespace"));

        let gw = GatewayEmbedder::new(
            "https://example.test/v1",
            "sk-test",
            "text-embedding-3-small",
            OPENAI_SMALL_EMBEDDING_DIM,
        );
        let gw_ddl = memories_schema(gw.dim());
        assert!(
            gw_ddl.contains("vector(1536)"),
            "gateway embedder ({}-d) must yield a vector(1536) column, got:\n{gw_ddl}",
            gw.dim()
        );
        assert!(!gw_ddl.contains("vector(1024)"));
    }
}
