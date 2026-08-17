/**
 * Durable Postgres storage for the TypeScript server — the swap behind the
 * `ponytail:` seams in `sessionStore.ts` and `admin.ts`.
 *
 * One class, {@link PostgresStore}, implements BOTH the {@link SessionStore}
 * (sessions / conversations / participants / messages) and the {@link AdminStore}
 * (connector configs, agent settings, indexing runs) — they live in the same
 * database and neither is useful without the other.
 *
 * Selected by `SMOOTH_AGENT_STORAGE=postgres` (see {@link resolveStorage}); with
 * the variable unset the server keeps its in-memory stores, unchanged.
 *
 * **Schema**: copied from the Rust reference adapter
 * (`rust/adapters/postgres/src/schema.rs`) so every server in this repo shares ONE
 * set of tables — same names, same columns, no per-language dialect. Every
 * statement is `CREATE ... IF NOT EXISTS`, so whichever server boots first creates
 * the tables and the rest no-op.
 *
 * Ownership follows the Rust model rather than inventing a column: a conversation's
 * owner is the email on its `user` participant row, which is exactly what the Rust
 * adapter's `list_conversations_by_org_and_user` filters on. The per-session bits
 * this server carries that Rust keeps in session metadata (`contactEmail`,
 * `otpVerified`, `currentStepId`) live in `conversation_sessions.metadata`.
 */

import { randomUUID } from 'node:crypto';
import { Pool } from 'pg';

import type { AdminStore, AgentSettings, ConnectorConfig, IndexingRun } from './admin.js';
import { DEFAULT_ORG_ID, type ConversationSummary, type MessageDirection, type SessionStore, type StoredMessage, type StoredSession } from './sessionStore.js';

/**
 * The DDL applied on connect. The OLTP + admin tables are verbatim from
 * `rust/adapters/postgres/src/schema.rs`; the one addition is the idempotent
 * `org_id` column on `indexing_runs`, which the Rust `IndexingStore` does not scope
 * by but the `/admin/*` API must (a run is listed per org). Adding it with
 * `ADD COLUMN IF NOT EXISTS` mirrors how the Rust schema back-fills
 * `knowledge_vectors.acl`, and leaves Rust's own queries untouched.
 */
const SCHEMA = `
CREATE TABLE IF NOT EXISTS conversations (
    id              TEXT PRIMARY KEY,
    platform        TEXT NOT NULL
                        CHECK (platform IN ('web', 'messenger', 'instagram', 'email', 'discord',
                        'phone', 'sms', 'slack', 'whatsapp', 'tiktok')),
    name            TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    metadata_json   JSONB NOT NULL DEFAULT '{}'::jsonb,
    analytics_json  JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
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
    metadata_json       JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_participants_conversation
    ON conversation_participants (conversation_id, created_at);
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
    metadata_json   JSONB NOT NULL DEFAULT '{}'::jsonb,
    analytics_json  JSONB NOT NULL DEFAULT '{}'::jsonb,
    seq             BIGSERIAL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation_seq
    ON conversation_messages (conversation_id, seq);

CREATE TABLE IF NOT EXISTS conversation_sessions (
    session_id           TEXT PRIMARY KEY,
    conversation_id      TEXT NOT NULL,
    organization_id      TEXT NOT NULL DEFAULT '',
    agent_id             TEXT NOT NULL,
    agent_name           TEXT NOT NULL,
    user_participant_id  TEXT NOT NULL,
    agent_participant_id TEXT NOT NULL,
    thread_id            TEXT NOT NULL,
    -- NULL passes a CHECK, so this constrains the value without making status required.
    status               TEXT CHECK (status IN ('active', 'idle', 'ended')),
    token_count          BIGINT,
    message_count        BIGINT,
    metadata             JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at             TIMESTAMPTZ,
    last_activity_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_sessions_conversation
    ON conversation_sessions (conversation_id, created_at);
CREATE INDEX IF NOT EXISTS idx_sessions_organization
    ON conversation_sessions (organization_id, created_at);

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
CREATE INDEX IF NOT EXISTS idx_indexing_runs_connector_started
    ON indexing_runs (connector_name, started_at DESC);
-- Org scope for the /admin/* run list. Idempotent so a database whose indexing_runs
-- was created by the Rust adapter gains the column in place.
ALTER TABLE indexing_runs ADD COLUMN IF NOT EXISTS org_id TEXT;
CREATE INDEX IF NOT EXISTS idx_indexing_runs_org_started
    ON indexing_runs (org_id, started_at DESC);
`;

/** The agent's display name, mirroring the in-memory store and the Rust `AGENT_NAME`. */
const AGENT_NAME = 'smooth-agent';

/**
 * The JSON held in `conversation_sessions.metadata`: the per-session bits
 * {@link StoredSession} carries that have no dedicated column in the shared schema.
 * Mirrors the Rust reference server's session metadata (where its `otpVerified` lives).
 */
interface SessionMetadata {
    contactEmail?: string;
    contactPhone?: string;
    otpVerified?: boolean;
    currentStepId?: string;
}

/** ISO-8601 in UTC, the shape every timestamp crosses this interface as. */
function iso(value: Date | string): string {
    return (value instanceof Date ? value : new Date(value)).toISOString();
}

/** Durable {@link SessionStore} + {@link AdminStore} on one Postgres pool. */
export class PostgresStore implements SessionStore, AdminStore {
    private constructor(private readonly pool: Pool) {}

    /** Connect and apply the schema (idempotent). */
    static async create(connectionString: string): Promise<PostgresStore> {
        const pool = new Pool({ connectionString });
        try {
            await pool.query(SCHEMA);
        } catch (error) {
            await pool.end();
            throw error;
        }
        return new PostgresStore(pool);
    }

    /** Release the connection pool. */
    async close(): Promise<void> {
        await this.pool.end();
    }

    // ── SessionStore ────────────────────────────────────────────────────────

    /**
     * Mint a session, binding to `conversationId` when it is known, in this org, AND
     * reachable by `userEmail`. Anything else — absent, unknown, another user's,
     * another org's — mints a fresh conversation through the identical branch, so a
     * caller cannot use resume as an oracle for which conversation ids exist.
     */
    async createSession(agentId: string, userName?: string, userEmail?: string, conversationId?: string, orgId: string = DEFAULT_ORG_ID): Promise<StoredSession> {
        const owner = userEmail?.trim() || undefined;

        let resumeId: string | undefined;
        let resumedOwner: string | undefined;
        if (conversationId) {
            const existing = await this.getConversation(conversationId, orgId);
            // Known AND reachable collapse into ONE condition on purpose — an unknown
            // conversation and someone else's take the identical branch below.
            if (existing && (!existing.userEmail || (owner !== undefined && existing.userEmail.toLowerCase() === owner.toLowerCase()))) {
                resumeId = conversationId;
                resumedOwner = existing.userEmail;
            }
        }

        const convId = resumeId ?? randomUUID();
        const session: StoredSession = {
            sessionId: randomUUID(),
            conversationId: convId,
            agentId: agentId && agentId.length > 0 ? agentId : randomUUID(),
            agentName: AGENT_NAME,
            userParticipantId: randomUUID(),
            agentParticipantId: randomUUID(),
            // A resumed conversation is in `orgId` by construction — getConversation only
            // matches rows whose organization_id equals it — so both branches stamp the same org.
            orgId,
            // On a resume the owner is the conversation's ORIGINAL owner, not this
            // caller's — re-deriving it would let a resume rewrite ownership.
            ...(resumeId ? (resumedOwner ? { userEmail: resumedOwner } : {}) : owner ? { userEmail: owner } : {}),
            // The caller's email doubles as the OTP delivery contact.
            ...(owner ? { contactEmail: owner } : {}),
        };

        const now = new Date().toISOString();
        const client = await this.pool.connect();
        try {
            await client.query('BEGIN');
            if (!resumeId) {
                // idempotency_key is the conversation id: unique per conversation, which
                // is what the (organization_id, idempotency_key) unique index wants.
                await client.query(
                    `INSERT INTO conversations (id, platform, name, organization_id, idempotency_key, created_at, updated_at)
                     VALUES ($1, 'web', '', $2, $1, $3, $3)`,
                    [convId, orgId, now],
                );
                // The `user` participant carries the owner email — the same column the
                // Rust adapter reads ownership from. Written ONCE, when the conversation
                // is minted; a resume never adds a second user participant, so resuming
                // can never re-home someone else's conversation nor claim an ownerless one.
                await client.query(
                    `INSERT INTO conversation_participants (id, conversation_id, organization_id, type, name, email, created_at, updated_at)
                     VALUES ($1, $2, $3, 'user', $4, $5, $6, $6)`,
                    [session.userParticipantId, convId, orgId, userName?.trim() || 'user', owner ?? null, now],
                );
                await client.query(
                    `INSERT INTO conversation_participants (id, conversation_id, organization_id, type, name, created_at, updated_at)
                     VALUES ($1, $2, $3, 'ai-agent', $4, $5, $5)`,
                    [session.agentParticipantId, convId, orgId, AGENT_NAME, now],
                );
            }
            const metadata: SessionMetadata = owner ? { contactEmail: owner } : {};
            await client.query(
                `INSERT INTO conversation_sessions
                    (session_id, conversation_id, organization_id, agent_id, agent_name, user_participant_id,
                     agent_participant_id, thread_id, status, metadata, created_at, updated_at, last_activity_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $2, 'active', $8::jsonb, $9, $9, $9)`,
                [session.sessionId, convId, orgId, session.agentId, session.agentName, session.userParticipantId, session.agentParticipantId, JSON.stringify(metadata), now],
            );
            await client.query('COMMIT');
        } catch (error) {
            await client.query('ROLLBACK');
            throw error;
        } finally {
            client.release();
        }
        return session;
    }

    /**
     * The session for `sessionId`, or null. The raw lookup primitive: ownership is
     * REPORTED (`userEmail`) but not enforced here, matching the in-memory store — the
     * dispatcher's `mayRead` is the gate. The owner is read from the conversation's
     * `user` participant rather than duplicated onto the session row, so there is one
     * source of truth and a resumed session reports the ORIGINAL owner.
     *
     * `orgId` MUST be populated for the gate to do its job. `mayRead` treats an absent
     * org as "unrecorded" and falls through to an ownership-only check, so a store that
     * omits it here does not fail loudly — it silently reopens the cross-org hole for
     * every ownerless conversation, on the one backend that holds several orgs' data.
     */
    async getSession(sessionId: string): Promise<StoredSession | null> {
        const { rows } = await this.pool.query(
            `SELECT s.conversation_id, s.agent_id, s.agent_name, s.user_participant_id, s.agent_participant_id,
                    s.organization_id,
                    COALESCE(s.metadata, '{}'::jsonb) AS metadata,
                    (SELECT p.email FROM conversation_participants p
                      WHERE p.conversation_id = s.conversation_id AND p.type = 'user'
                      ORDER BY p.created_at, p.id LIMIT 1) AS owner_email
               FROM conversation_sessions s
              WHERE s.session_id = $1`,
            [sessionId],
        );
        const row = rows[0];
        if (!row) return null;
        const metadata = (row.metadata ?? {}) as SessionMetadata;
        return {
            sessionId,
            conversationId: row.conversation_id,
            agentId: row.agent_id,
            agentName: row.agent_name,
            userParticipantId: row.user_participant_id,
            agentParticipantId: row.agent_participant_id,
            ...(row.organization_id ? { orgId: row.organization_id as string } : {}),
            ...(row.owner_email ? { userEmail: row.owner_email as string } : {}),
            ...(metadata.contactEmail ? { contactEmail: metadata.contactEmail } : {}),
            ...(metadata.contactPhone ? { contactPhone: metadata.contactPhone } : {}),
            ...(metadata.otpVerified ? { otpVerified: true } : {}),
            ...(metadata.currentStepId ? { currentStepId: metadata.currentStepId } : {}),
        };
    }

    /**
     * A conversation by id within `orgId`, or null. A conversation in ANOTHER org
     * returns null too — indistinguishable from one that never existed, so the id
     * space cannot be probed across orgs.
     */
    async getConversation(
        conversationId: string,
        orgId: string = DEFAULT_ORG_ID,
    ): Promise<{ conversationId: string; userEmail: string | undefined; orgId?: string } | null> {
        const { rows } = await this.pool.query(
            `SELECT c.organization_id,
                    (SELECT p.email FROM conversation_participants p
                      WHERE p.conversation_id = c.id AND p.type = 'user'
                      ORDER BY p.created_at, p.id LIMIT 1) AS owner_email
               FROM conversations c
              WHERE c.id = $1 AND c.organization_id = $2`,
            [conversationId, orgId],
        );
        const row = rows[0];
        if (!row) return null;
        return { conversationId, userEmail: (row.owner_email as string | null) ?? undefined, orgId: row.organization_id as string };
    }

    /**
     * Append a message and bump the conversation's last-activity time (the
     * `listConversations` sort key) in one transaction, so the two never disagree.
     */
    async appendMessage(conversationId: string, direction: MessageDirection, text: string): Promise<StoredMessage> {
        const message: StoredMessage = { id: randomUUID(), conversationId, direction, text, createdAt: new Date().toISOString() };
        // The stored `content` is the same {items, text} the Rust MessageContent
        // serializes to, so a row written here reads back in every other server.
        const content = JSON.stringify({ items: [{ type: 'text', text }], text });

        const client = await this.pool.connect();
        try {
            await client.query('BEGIN');
            await client.query(
                `INSERT INTO conversation_messages (id, organization_id, conversation_id, direction, content, created_at)
                 VALUES ($1, (SELECT organization_id FROM conversations WHERE id = $2), $2, $3, $4::jsonb, $5)`,
                [message.id, conversationId, direction, content, message.createdAt],
            );
            await client.query('UPDATE conversations SET updated_at = $2 WHERE id = $1', [conversationId, message.createdAt]);
            await client.query('COMMIT');
        } catch (error) {
            await client.query('ROLLBACK');
            throw error;
        } finally {
            client.release();
        }
        return message;
    }

    /** The most recent `limit` messages for a conversation, oldest first. */
    async listMessages(conversationId: string, limit: number): Promise<StoredMessage[]> {
        const { rows } = await this.pool.query(
            `SELECT id, direction, content->>'text' AS text, created_at
               FROM (SELECT id, direction, content, created_at, seq
                       FROM conversation_messages
                      WHERE conversation_id = $1
                      ORDER BY seq DESC LIMIT $2) recent
              ORDER BY recent.seq ASC`,
            [conversationId, limit > 0 ? limit : Number.MAX_SAFE_INTEGER],
        );
        return rows.map((row) => ({
            id: row.id,
            conversationId,
            direction: row.direction as MessageDirection,
            text: (row.text as string | null) ?? '',
            createdAt: iso(row.created_at),
        }));
    }

    /**
     * A summary per conversation in `orgId` reachable by `userEmail` that has at least
     * one message.
     *
     * SECURITY: both filters — org and owner — are in the SELECT, not applied to an
     * already-truncated page. Filtering after a limit hands back short or empty pages
     * that read as "no conversations" rather than as a bug.
     *
     * `userEmail === undefined` is unscoped-by-OWNER, reserved for auth-disabled
     * single-tenant servers. It is not unscoped by ORG: widening ownership must never
     * widen tenancy.
     */
    async listConversations(userEmail: string | undefined, orgId: string = DEFAULT_ORG_ID): Promise<ConversationSummary[]> {
        const { rows } = await this.pool.query(
            `SELECT c.id,
                    c.updated_at,
                    (SELECT count(*) FROM conversation_messages m WHERE m.conversation_id = c.id) AS message_count,
                    (SELECT m.content->>'text' FROM conversation_messages m
                      WHERE m.conversation_id = c.id AND m.direction = 'inbound'
                      ORDER BY m.seq ASC LIMIT 1) AS first_inbound
               FROM conversations c
              WHERE c.organization_id = $1
                AND EXISTS (SELECT 1 FROM conversation_messages m WHERE m.conversation_id = c.id)
                AND ($2::boolean OR EXISTS (
                      SELECT 1 FROM conversation_participants p
                       WHERE p.conversation_id = c.id AND p.type = 'user'
                         AND COALESCE(btrim(p.email), '') = ''
                    ) OR EXISTS (
                      SELECT 1 FROM conversation_participants p
                       WHERE p.conversation_id = c.id AND p.type = 'user'
                         AND lower(btrim(p.email)) = lower(btrim($3))))`,
            [orgId, userEmail === undefined, userEmail ?? ''],
        );
        return rows.map((row) => ({
            conversationId: row.id,
            updatedAt: iso(row.updated_at),
            messageCount: Number(row.message_count),
            ...(row.first_inbound ? { firstInboundText: row.first_inbound as string } : {}),
        }));
    }

    /** Persist a session's workflow step id. A no-op for an unknown session. */
    async setCurrentStep(sessionId: string, currentStepId: string): Promise<void> {
        await this.mergeMetadata(sessionId, { currentStepId });
    }

    /** Persist a session's OTP-verified bit. A no-op for an unknown session. */
    async setAuthenticated(sessionId: string, verified: boolean): Promise<void> {
        await this.mergeMetadata(sessionId, { otpVerified: verified });
    }

    /**
     * Merge `patch` into a session's metadata JSON. `||` on jsonb is a shallow merge,
     * which is all this flat object needs — and it leaves the other keys (the contact
     * email, the sibling flag) alone instead of clobbering them.
     */
    private async mergeMetadata(sessionId: string, patch: SessionMetadata): Promise<void> {
        await this.pool.query(
            `UPDATE conversation_sessions
                SET metadata = COALESCE(metadata, '{}'::jsonb) || $2::jsonb, updated_at = now()
              WHERE session_id = $1`,
            [sessionId, JSON.stringify(patch)],
        );
    }

    // ── AdminStore ──────────────────────────────────────────────────────────

    /** An org's connectors, sorted by name. */
    async listConnectors(orgId: string): Promise<ConnectorConfig[]> {
        const { rows } = await this.pool.query(
            `SELECT id, name, kind, config, enabled, created_at, updated_at
               FROM connector_configs WHERE org_id = $1 ORDER BY name`,
            [orgId],
        );
        return rows.map((row) => this.toConnector(row, orgId));
    }

    /**
     * An org's connector, or undefined when unknown. A connector belonging to ANOTHER
     * org returns undefined too — the caller renders the same 404, so the id space
     * cannot be probed across orgs.
     */
    async getConnector(orgId: string, id: string): Promise<ConnectorConfig | undefined> {
        const { rows } = await this.pool.query(
            `SELECT id, name, kind, config, enabled, created_at, updated_at
               FROM connector_configs WHERE org_id = $1 AND id = $2`,
            [orgId, id],
        );
        return rows[0] ? this.toConnector(rows[0], orgId) : undefined;
    }

    private toConnector(row: Record<string, unknown>, orgId: string): ConnectorConfig {
        return {
            id: row.id as string,
            name: row.name as string,
            kind: row.kind as string,
            config: (row.config ?? {}) as Record<string, unknown>,
            enabled: row.enabled as boolean,
            createdAt: iso(row.created_at as Date),
            updatedAt: iso(row.updated_at as Date),
            orgId,
        };
    }

    /** Insert or update a connector in its org. */
    async putConnector(connector: ConnectorConfig): Promise<void> {
        await this.pool.query(
            `INSERT INTO connector_configs (org_id, id, name, kind, config, enabled, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $8)
             ON CONFLICT (org_id, id) DO UPDATE SET
                 name = EXCLUDED.name, kind = EXCLUDED.kind, config = EXCLUDED.config,
                 enabled = EXCLUDED.enabled, updated_at = EXCLUDED.updated_at`,
            [connector.orgId, connector.id, connector.name, connector.kind, JSON.stringify(connector.config), connector.enabled, connector.createdAt, connector.updatedAt],
        );
    }

    /** Remove an org's connector, reporting whether it existed. Cross-org deletes nothing. */
    async deleteConnector(orgId: string, id: string): Promise<boolean> {
        const result = await this.pool.query('DELETE FROM connector_configs WHERE org_id = $1 AND id = $2', [orgId, id]);
        return (result.rowCount ?? 0) > 0;
    }

    /** An org's settings, or undefined when unset (the caller substitutes defaults). */
    async getSettings(orgId: string): Promise<AgentSettings | undefined> {
        const { rows } = await this.pool.query('SELECT model, system_prompt, default_tools, updated_at FROM agent_settings WHERE org_id = $1', [orgId]);
        const row = rows[0];
        if (!row) return undefined;
        return {
            orgId,
            model: row.model as string,
            systemPrompt: row.system_prompt as string,
            defaultTools: (row.default_tools ?? []) as string[],
            updatedAt: iso(row.updated_at as Date),
        };
    }

    /** Write an org's settings (one row per org). */
    async putSettings(settings: AgentSettings): Promise<void> {
        await this.pool.query(
            `INSERT INTO agent_settings (org_id, model, system_prompt, default_tools, updated_at)
             VALUES ($1, $2, $3, $4::jsonb, $5)
             ON CONFLICT (org_id) DO UPDATE SET
                 model = EXCLUDED.model, system_prompt = EXCLUDED.system_prompt,
                 default_tools = EXCLUDED.default_tools, updated_at = EXCLUDED.updated_at`,
            [settings.orgId, settings.model, settings.systemPrompt, JSON.stringify(settings.defaultTools), settings.updatedAt],
        );
    }

    /** An org's indexing runs, oldest first (insertion order, like the in-memory array). */
    async listRuns(orgId: string): Promise<IndexingRun[]> {
        const { rows } = await this.pool.query(
            `SELECT id, connector_name, status, started_at, finished_at, documents_seen,
                    chunks_indexed, documents_skipped, error
               FROM indexing_runs WHERE org_id = $1 ORDER BY started_at, id`,
            [orgId],
        );
        return rows.map((row) => ({
            id: row.id as string,
            connectorName: row.connector_name as string,
            status: row.status as string,
            startedAt: iso(row.started_at as Date),
            finishedAt: row.finished_at ? iso(row.finished_at as Date) : null,
            documentsSeen: Number(row.documents_seen),
            chunksIndexed: Number(row.chunks_indexed),
            documentsSkipped: Number(row.documents_skipped),
            error: (row.error as string | null) ?? null,
            orgId,
        }));
    }

    /** Insert or update an indexing run. */
    async recordRun(run: IndexingRun): Promise<void> {
        await this.pool.query(
            `INSERT INTO indexing_runs (id, org_id, connector_name, status, started_at, finished_at,
                                        documents_seen, chunks_indexed, documents_skipped, error)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO UPDATE SET
                 status = EXCLUDED.status, finished_at = EXCLUDED.finished_at,
                 documents_seen = EXCLUDED.documents_seen, chunks_indexed = EXCLUDED.chunks_indexed,
                 documents_skipped = EXCLUDED.documents_skipped, error = EXCLUDED.error`,
            [run.id, run.orgId, run.connectorName, run.status, run.startedAt, run.finishedAt, run.documentsSeen, run.chunksIndexed, run.documentsSkipped, run.error],
        );
    }
}

/**
 * The storage backend named by `SMOOTH_AGENT_STORAGE` — the same contract the Rust
 * server uses:
 *
 * - `memory` (or unset) → `undefined`; the caller keeps its in-memory stores.
 * - `postgres` → a {@link PostgresStore} on `SMOOTH_AGENT_DATABASE_URL` (falling back
 *   to `DATABASE_URL`, but only once `postgres` has been asked for explicitly — an
 *   ambient `DATABASE_URL` alone can never change where data goes).
 *
 * Any other value throws rather than silently falling back to memory: a host that
 * asked for durability and quietly got none is the failure mode worth shouting about.
 */
export async function resolveStorage(env: NodeJS.ProcessEnv = process.env): Promise<PostgresStore | undefined> {
    const backend = env.SMOOTH_AGENT_STORAGE?.trim() ?? '';
    if (backend === '' || backend === 'memory') return undefined;
    if (backend !== 'postgres') {
        throw new Error(`unknown SMOOTH_AGENT_STORAGE '${backend}' (want memory or postgres)`);
    }
    const connectionString = env.SMOOTH_AGENT_DATABASE_URL?.trim() || env.DATABASE_URL?.trim();
    if (!connectionString) {
        throw new Error('SMOOTH_AGENT_STORAGE=postgres but neither SMOOTH_AGENT_DATABASE_URL nor DATABASE_URL is set');
    }
    return PostgresStore.create(connectionString);
}
