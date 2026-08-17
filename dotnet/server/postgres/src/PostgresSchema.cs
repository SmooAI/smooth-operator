namespace SmooAI.SmoothOperator.Server.Postgres;

/// <summary>
/// The SHARED relational schema, applied on store init. Byte-for-byte the shape in
/// <c>rust/adapters/postgres/src/schema.rs</c> (the source of truth) and the Go store's copy of
/// it, so a row written by any of the five servers reads back correctly in the others.
/// </summary>
/// <remarks>
/// <para>
/// This host previously INVENTED its own tables — a narrower <c>conversation_sessions</c> plus
/// <c>conversation_identity_state</c> and <c>conversation_workflow_state</c> side tables — and no
/// <c>conversations</c> or <c>conversation_participants</c> at all. The per-session bits those side
/// tables held now live in <c>conversation_sessions.metadata</c> under the same keys Rust and Go
/// use (<c>contactEmail</c>, <c>otpVerified</c>, <c>currentStepId</c>). <see cref="Migration"/>
/// carries the old data across before the side tables are dropped.
/// </para>
/// <para>
/// <c>organization_id</c> is written as <c>''</c> here: this host's <c>ISessionStore</c> surface
/// carries no org, so it uses the column's schema default rather than inventing a second notion of
/// tenancy. The columns exist so the rows are readable by the org-aware servers.
/// </para>
/// </remarks>
internal static class PostgresSchema
{
    /// <summary>
    /// Phase 1 of 3: the OLTP tables. No pgvector dependency, so they apply unconditionally.
    /// <para>
    /// Indexes deliberately live in <see cref="Indexes"/> instead of here. On a database created by
    /// the OLD C# shape these tables already exist, so <c>CREATE TABLE IF NOT EXISTS</c> is a no-op
    /// and leaves them NARROW — an index defined alongside the table would then reference a column
    /// that <see cref="Migration"/> has not added yet and the whole init would fail. Creating tables,
    /// then widening, then indexing removes that ordering trap for every future column too.
    /// </para>
    /// </summary>
    internal const string Tables = """
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
            seq             BIGSERIAL,
            created_at      TIMESTAMPTZ NOT NULL,
            updated_at      TIMESTAMPTZ
        );

        CREATE TABLE IF NOT EXISTS conversation_sessions (
            session_id           TEXT PRIMARY KEY,
            conversation_id      TEXT NOT NULL,
            organization_id      TEXT NOT NULL DEFAULT '',
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
        """;

    /// <summary>
    /// Carry a database written by the PREVIOUS C# shape onto the shared one, then drop what this
    /// host invented. Idempotent and safe on a fresh database: every statement is
    /// <c>IF NOT EXISTS</c>/<c>IF EXISTS</c>, and the side tables are CREATEd (empty) before being
    /// read so the backfills are a no-op rather than an error when they never existed.
    /// </summary>
    internal const string Migration = """
        -- The old conversation_sessions was narrower; widen it in place rather than forcing a
        -- destructive recreate. (A fresh database already has these from the DDL above.)
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS organization_id      TEXT NOT NULL DEFAULT '';
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS thread_id            TEXT NOT NULL DEFAULT '';
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS status               TEXT;
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS token_count          BIGINT;
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS message_count        BIGINT;
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS metadata             JSONB;
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS updated_at           TIMESTAMPTZ;
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS ended_at             TIMESTAMPTZ;
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS last_activity_at     TIMESTAMPTZ;
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS user_email           TEXT;

        ALTER TABLE conversation_messages ADD COLUMN IF NOT EXISTS external_id     TEXT;
        ALTER TABLE conversation_messages ADD COLUMN IF NOT EXISTS organization_id TEXT;
        ALTER TABLE conversation_messages ADD COLUMN IF NOT EXISTS from_ref        JSONB;
        ALTER TABLE conversation_messages ADD COLUMN IF NOT EXISTS to_ref          JSONB;
        ALTER TABLE conversation_messages ADD COLUMN IF NOT EXISTS metadata_json   JSONB;
        ALTER TABLE conversation_messages ADD COLUMN IF NOT EXISTS analytics_json  JSONB;
        ALTER TABLE conversation_messages ADD COLUMN IF NOT EXISTS updated_at      TIMESTAMPTZ;

        -- Created empty when absent purely so the backfills below are a no-op instead of an error
        -- on a database that never ran the old C# shape. Dropped again at the end either way.
        CREATE TABLE IF NOT EXISTS conversation_identity_state (
            conversation_id TEXT PRIMARY KEY,
            otp_verified    BOOLEAN NOT NULL,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE TABLE IF NOT EXISTS conversation_workflow_state (
            conversation_id TEXT PRIMARY KEY,
            step_id         TEXT NOT NULL,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
        );

        -- Side tables → conversation_sessions.metadata, under the keys Rust/Go already use.
        -- `||` is a shallow merge, so each backfill leaves the sibling keys alone.
        UPDATE conversation_sessions s
           SET metadata = coalesce(s.metadata, '{}'::jsonb) || jsonb_build_object('otpVerified', i.otp_verified)
          FROM conversation_identity_state i
         WHERE i.conversation_id = s.conversation_id;

        UPDATE conversation_sessions s
           SET metadata = coalesce(s.metadata, '{}'::jsonb) || jsonb_build_object('currentStepId', w.step_id)
          FROM conversation_workflow_state w
         WHERE w.conversation_id = s.conversation_id;

        -- user_email was a column this host added that the shared schema does not have: the owner
        -- lives on the conversation's user participant, so a resumed session reports the ORIGINAL
        -- owner from one source of truth (matching the Go store).
        UPDATE conversation_sessions s
           SET metadata = coalesce(s.metadata, '{}'::jsonb) || jsonb_build_object('contactEmail', s.user_email)
         WHERE s.user_email IS NOT NULL;

        -- A conversations row for every legacy session (the old shape had no conversations table).
        INSERT INTO conversations (id, platform, name, organization_id, idempotency_key, created_at, updated_at)
        SELECT DISTINCT ON (s.conversation_id)
               s.conversation_id, 'smooth-operator', 'conversation', '', s.conversation_id,
               coalesce(s.created_at, now()), coalesce(s.created_at, now())
          FROM conversation_sessions s
         WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = s.conversation_id)
         ORDER BY s.conversation_id, s.created_at;

        -- ...and a user participant carrying the email the old user_email column held.
        INSERT INTO conversation_participants (id, conversation_id, organization_id, type, name, email, created_at, updated_at)
        SELECT DISTINCT ON (s.conversation_id)
               s.user_participant_id, s.conversation_id, '', 'user', 'user', s.user_email,
               coalesce(s.created_at, now()), coalesce(s.created_at, now())
          FROM conversation_sessions s
         WHERE s.user_email IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM conversation_participants p
                            WHERE p.conversation_id = s.conversation_id AND p.type = 'user')
         ORDER BY s.conversation_id, s.created_at;

        DROP TABLE IF EXISTS conversation_identity_state;
        DROP TABLE IF EXISTS conversation_workflow_state;
        ALTER TABLE conversation_sessions DROP COLUMN IF EXISTS user_email;
        """;

    /// <summary>
    /// Phase 3 of 3: the indexes, applied only once <see cref="Migration"/> has widened the tables.
    /// A legacy database's <c>conversation_sessions</c> has no <c>organization_id</c> until then, so
    /// creating <c>idx_sessions_organization</c> any earlier fails the whole init.
    /// </summary>
    internal const string Indexes = """
        -- Enforces conversation create idempotency on (org, idempotencyKey).
        CREATE UNIQUE INDEX IF NOT EXISTS uq_conversations_org_idem
            ON conversations (organization_id, idempotency_key);
        CREATE INDEX IF NOT EXISTS idx_conversations_org_created
            ON conversations (organization_id, created_at DESC);

        CREATE INDEX IF NOT EXISTS idx_participants_conversation
            ON conversation_participants (conversation_id, created_at);
        -- Resolve a returning user by external identity within a conversation.
        CREATE INDEX IF NOT EXISTS idx_participants_external
            ON conversation_participants (conversation_id, external_id);

        CREATE INDEX IF NOT EXISTS idx_messages_conversation_seq
            ON conversation_messages (conversation_id, seq);

        CREATE INDEX IF NOT EXISTS idx_sessions_conversation
            ON conversation_sessions (conversation_id, created_at);
        CREATE INDEX IF NOT EXISTS idx_sessions_organization
            ON conversation_sessions (organization_id, created_at);
        """;
}
