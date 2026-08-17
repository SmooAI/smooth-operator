"""Durable Postgres storage for the Python server — the swap behind the ``ponytail:``
seams in :mod:`session_store` and :mod:`admin`.

One class, :class:`PostgresStore`, implements BOTH the :class:`SessionStore`
(sessions / conversations / participants / messages) and the :class:`AdminStore`
(connector configs, agent settings, indexing runs) — they live in the same database
and neither is useful without the other.

Selected by ``SMOOTH_AGENT_STORAGE=postgres`` (see :func:`resolve_storage`); with the
variable unset the server keeps its in-memory stores, unchanged.

**Schema**: copied from the Rust reference adapter
(``rust/adapters/postgres/src/schema.rs``) so every server in this repo shares ONE set
of tables — same names, same columns, no per-language dialect. Every statement is
``CREATE ... IF NOT EXISTS``, so whichever server boots first creates the tables and
the rest no-op.

Ownership follows the Rust model rather than inventing a column: a conversation's
owner is the email on its ``user`` participant row, which is exactly what the Rust
adapter's ``list_conversations_by_org_and_user`` filters on. The per-conversation
workflow step lives in ``conversations.metadata_json`` and the per-session OTP bit in
``conversation_sessions.metadata`` — the same place the Rust reference server keeps
``otpVerified``.

``asyncpg`` is an OPTIONAL dependency (the ``postgres`` extra): importing this module
is what pulls it in, and nothing does that unless ``SMOOTH_AGENT_STORAGE=postgres``.
"""

from __future__ import annotations

import json
import os
import uuid
from datetime import datetime, timezone
from typing import Any, Optional

import asyncpg

from .auth import normalize_email
from .session_store import (
    AGENT_NAME,
    DEFAULT_ORG_ID,
    ConversationSummary,
    MessageDirection,
    SessionStore,
    StoredMessage,
    StoredSession,
)

#: The DDL applied on connect. The OLTP + admin tables are verbatim from
#: ``rust/adapters/postgres/src/schema.rs``; the one addition is the idempotent
#: ``org_id`` column on ``indexing_runs``, which the Rust ``IndexingStore`` does not
#: scope by but the ``/admin/*`` API must (a run is listed per org). Adding it with
#: ``ADD COLUMN IF NOT EXISTS`` mirrors how the Rust schema back-fills
#: ``knowledge_vectors.acl``, and leaves Rust's own queries untouched.
SCHEMA = """
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
    -- Nullable: a session with no caller-supplied agent has NO agent (th-68897a).
    agent_id             TEXT,
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
"""


def _now() -> datetime:
    return datetime.now(timezone.utc)


def _iso(value: Optional[datetime]) -> Optional[str]:
    return value.isoformat() if value is not None else None


class PostgresStore(SessionStore):
    """Durable :class:`SessionStore` + admin store on one asyncpg pool."""

    def __init__(self, pool: asyncpg.Pool) -> None:
        self._pool = pool

    @classmethod
    async def create(cls, dsn: str) -> "PostgresStore":
        """Connect and apply the schema (idempotent)."""
        pool = await asyncpg.create_pool(dsn)
        assert pool is not None
        try:
            await pool.execute(SCHEMA)
        except BaseException:
            await pool.close()
            raise
        return cls(pool)

    async def close(self) -> None:
        """Release the connection pool."""
        await self._pool.close()

    # ── SessionStore ────────────────────────────────────────────────────────

    async def create_session(
        self,
        agent_id: str,
        user_name: str | None,
        user_email: str | None,
        conversation_id: str | None = None,
        *,
        owner_email: str | None = None,
        enforced: bool = False,
        org_id: str = DEFAULT_ORG_ID,
    ) -> StoredSession:
        """Mint a session, binding to ``conversation_id`` when it is known, in this
        org, AND reachable by ``owner_email``. Anything else — absent, unknown,
        another user's, another org's — mints a fresh conversation through the
        identical branch, so a caller cannot use resume as an oracle for which
        conversation ids exist."""
        owner = normalize_email(owner_email)

        resume_id: str | None = None
        resumed_owner: str | None = None
        if conversation_id:
            existing = await self._conversation_owner(conversation_id, org_id)
            if existing is not None:
                found, existing_owner = existing
                # Known AND reachable collapse into ONE condition on purpose. With
                # auth disabled (``enforced=False``) ownership is not consulted at all.
                if found and (not enforced or existing_owner in (None, owner)):
                    resume_id, resumed_owner = conversation_id, existing_owner

        conv_id = resume_id or str(uuid.uuid4())
        session = StoredSession(
            session_id=str(uuid.uuid4()),
            conversation_id=conv_id,
            agent_id=(agent_id or "").strip() or None,
            agent_name=AGENT_NAME,
            user_participant_id=str(uuid.uuid4()),
            agent_participant_id=str(uuid.uuid4()),
            contact_email=(user_email.strip() or None) if isinstance(user_email, str) else None,
            # On a resume the session inherits the conversation's EXISTING owner rather
            # than re-stamping it, so resuming can never quietly transfer a conversation.
            owner_email=resumed_owner if resume_id else owner,
            # A resumed conversation is in org_id by construction — _conversation_owner
            # only matches rows whose organization_id equals it — so both branches stamp
            # the same org.
            owner_org=org_id,
        )

        now = _now()
        async with self._pool.acquire() as conn:
            async with conn.transaction():
                if resume_id is None:
                    # idempotency_key is the conversation id: unique per conversation,
                    # which is what the (organization_id, idempotency_key) index wants.
                    await conn.execute(
                        """INSERT INTO conversations
                               (id, platform, name, organization_id, idempotency_key, created_at, updated_at)
                           VALUES ($1, 'web', '', $2, $1, $3, $3)""",
                        conv_id,
                        org_id,
                        now,
                    )
                    # The `user` participant carries the owner email — the same column
                    # the Rust adapter reads ownership from. Written ONCE, when the
                    # conversation is minted; a resume never adds a second user
                    # participant, so resuming can never re-home someone else's
                    # conversation nor claim an ownerless one.
                    await conn.execute(
                        """INSERT INTO conversation_participants
                               (id, conversation_id, organization_id, type, name, email, created_at, updated_at)
                           VALUES ($1, $2, $3, 'user', $4, $5, $6, $6)""",
                        session.user_participant_id,
                        conv_id,
                        org_id,
                        (user_name or "").strip() or "user",
                        owner,
                        now,
                    )
                    await conn.execute(
                        """INSERT INTO conversation_participants
                               (id, conversation_id, organization_id, type, name, created_at, updated_at)
                           VALUES ($1, $2, $3, 'ai-agent', $4, $5, $5)""",
                        session.agent_participant_id,
                        conv_id,
                        org_id,
                        AGENT_NAME,
                        now,
                    )
                metadata = {"contactEmail": session.contact_email} if session.contact_email else {}
                await conn.execute(
                    """INSERT INTO conversation_sessions
                           (session_id, conversation_id, organization_id, agent_id, agent_name,
                            user_participant_id, agent_participant_id, thread_id, status, metadata,
                            created_at, updated_at, last_activity_at)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $2, 'active', $8::jsonb, $9, $9, $9)""",
                    session.session_id,
                    conv_id,
                    org_id,
                    session.agent_id,
                    session.agent_name,
                    session.user_participant_id,
                    session.agent_participant_id,
                    json.dumps(metadata),
                    now,
                )
        return session

    async def _conversation_owner(self, conversation_id: str, org_id: str) -> Optional[tuple[bool, str | None]]:
        """``(True, owner)`` when the conversation exists in ``org_id`` (owner ``None``
        when ownerless), else ``None``. A conversation in ANOTHER org reports ``None``
        too — indistinguishable from one that never existed."""
        row = await self._pool.fetchrow(
            """SELECT (SELECT p.email FROM conversation_participants p
                        WHERE p.conversation_id = c.id AND p.type = 'user'
                        ORDER BY p.created_at, p.id LIMIT 1) AS owner_email
                 FROM conversations c
                WHERE c.id = $1 AND c.organization_id = $2""",
            conversation_id,
            org_id,
        )
        if row is None:
            return None
        return True, normalize_email(row["owner_email"])

    async def get_session(self, session_id: str) -> StoredSession | None:
        """The session for ``session_id``, or ``None``.

        The raw lookup primitive: ownership is REPORTED (``owner_email``) but not
        enforced here, matching the in-memory store — the dispatcher is the gate. The
        owner is read from the conversation's ``user`` participant rather than
        duplicated onto the session row, so there is one source of truth and a resumed
        session reports the ORIGINAL owner.

        ``owner_org`` MUST be populated for the gate to do its job: the dispatcher
        treats an unrecorded org as "fall through to ownership", so a store that leaves
        it ``None`` here does not fail loudly — it silently reopens the cross-org hole
        for every ownerless conversation, on the one backend that holds several orgs'
        data."""
        row = await self._pool.fetchrow(
            """SELECT s.conversation_id, s.agent_id, s.agent_name, s.user_participant_id,
                      s.agent_participant_id, s.organization_id,
                      COALESCE(s.metadata, '{}'::jsonb) AS metadata,
                      (SELECT p.email FROM conversation_participants p
                        WHERE p.conversation_id = s.conversation_id AND p.type = 'user'
                        ORDER BY p.created_at, p.id LIMIT 1) AS owner_email
                 FROM conversation_sessions s
                WHERE s.session_id = $1""",
            session_id,
        )
        if row is None:
            return None
        metadata = json.loads(row["metadata"]) if isinstance(row["metadata"], str) else dict(row["metadata"] or {})
        return StoredSession(
            session_id=session_id,
            conversation_id=row["conversation_id"],
            agent_id=row["agent_id"],
            agent_name=row["agent_name"],
            user_participant_id=row["user_participant_id"],
            agent_participant_id=row["agent_participant_id"],
            contact_email=metadata.get("contactEmail"),
            owner_email=normalize_email(row["owner_email"]),
            owner_org=row["organization_id"] or None,
        )

    async def append_message(self, conversation_id: str, direction: MessageDirection, text: str) -> StoredMessage:
        """Append a message and bump the conversation's last-activity time (the
        ``list_conversations`` sort key) in one transaction, so the two never disagree."""
        message = StoredMessage(str(uuid.uuid4()), conversation_id, direction, text, _now())
        # The stored `content` is the same {items, text} the Rust MessageContent
        # serializes to, so a row written here reads back in every other server.
        content = json.dumps({"items": [{"type": "text", "text": text}], "text": text})
        async with self._pool.acquire() as conn:
            async with conn.transaction():
                await conn.execute(
                    """INSERT INTO conversation_messages
                           (id, organization_id, conversation_id, direction, content, created_at)
                       VALUES ($1, (SELECT organization_id FROM conversations WHERE id = $2), $2, $3, $4::jsonb, $5)""",
                    message.id,
                    conversation_id,
                    direction.value,
                    content,
                    message.created_at,
                )
                await conn.execute(
                    "UPDATE conversations SET updated_at = $2 WHERE id = $1",
                    conversation_id,
                    message.created_at,
                )
        return message

    async def list_messages(self, conversation_id: str, limit: int) -> list[StoredMessage]:
        """The most recent ``limit`` messages for a conversation, oldest first. A
        non-positive limit means "all", matching the in-memory store."""
        rows = await self._pool.fetch(
            """SELECT id, direction, content->>'text' AS text, created_at
                 FROM (SELECT id, direction, content, created_at, seq
                         FROM conversation_messages
                        WHERE conversation_id = $1
                        ORDER BY seq DESC LIMIT $2) recent
                ORDER BY recent.seq ASC""",
            conversation_id,
            limit if limit > 0 else 2**31 - 1,
        )
        return [
            StoredMessage(
                row["id"],
                conversation_id,
                MessageDirection(row["direction"]),
                row["text"] or "",
                row["created_at"],
            )
            for row in rows
        ]

    async def list_conversations(
        self,
        user_email: str | None,
        *,
        enforced: bool = False,
        org_id: str = DEFAULT_ORG_ID,
    ) -> list[ConversationSummary]:
        """A summary per conversation in ``org_id`` reachable by ``user_email`` that has
        at least one message.

        SECURITY: both filters — org and owner — are in the SELECT, not applied to an
        already-truncated page. Filtering after a limit hands back short or empty pages
        that read as "no conversations" rather than as a bug.

        ``enforced=False`` is the single-tenant, auth-disabled flavor: unscoped by
        OWNER. It is NOT unscoped by ORG — widening ownership must never widen tenancy."""
        scope = normalize_email(user_email)
        rows = await self._pool.fetch(
            """SELECT c.id,
                      c.updated_at,
                      (SELECT count(*) FROM conversation_messages m WHERE m.conversation_id = c.id) AS message_count,
                      (SELECT m.content->>'text' FROM conversation_messages m
                        WHERE m.conversation_id = c.id AND m.direction = 'inbound'
                        ORDER BY m.seq ASC LIMIT 1) AS first_inbound
                 FROM conversations c
                WHERE c.organization_id = $1
                  AND EXISTS (SELECT 1 FROM conversation_messages m WHERE m.conversation_id = c.id)
                  AND (NOT $2::boolean OR EXISTS (
                        SELECT 1 FROM conversation_participants p
                         WHERE p.conversation_id = c.id AND p.type = 'user'
                           AND (COALESCE(btrim(p.email), '') = ''
                                OR ($3::text IS NOT NULL AND lower(btrim(p.email)) = lower(btrim($3))))))""",
            org_id,
            enforced,
            scope,
        )
        return [
            ConversationSummary(
                conversation_id=row["id"],
                updated_at=row["updated_at"],
                message_count=int(row["message_count"]),
                first_inbound_text=row["first_inbound"],
            )
            for row in rows
        ]

    async def get_current_step_id(self, conversation_id: str) -> str | None:
        """The conversation's workflow-step pointer (``None`` = fresh start)."""
        return await self._pool.fetchval(
            "SELECT metadata_json->>'currentStepId' FROM conversations WHERE id = $1",
            conversation_id,
        )

    async def set_current_step_id(self, conversation_id: str, step_id: str | None) -> None:
        """Persist (or clear) the conversation's workflow-step pointer. ``||`` on jsonb
        is a shallow merge, and ``- 'currentStepId'`` removes just that key, so neither
        write disturbs the rest of the object."""
        if step_id is None:
            await self._pool.execute(
                """UPDATE conversations
                      SET metadata_json = COALESCE(metadata_json, '{}'::jsonb) - 'currentStepId'
                    WHERE id = $1""",
                conversation_id,
            )
            return
        await self._pool.execute(
            """UPDATE conversations
                  SET metadata_json = COALESCE(metadata_json, '{}'::jsonb) || $2::jsonb
                WHERE id = $1""",
            conversation_id,
            json.dumps({"currentStepId": step_id}),
        )

    async def is_session_authenticated(self, session_id: str) -> bool:
        """Whether the caller completed OTP verification for this session. ``False`` for
        an unknown or unverified session."""
        value = await self._pool.fetchval(
            "SELECT metadata->>'otpVerified' FROM conversation_sessions WHERE session_id = $1",
            session_id,
        )
        return value == "true"

    async def set_session_authenticated(self, session_id: str, verified: bool) -> None:
        """Mark this session identity-verified (or clear it). A no-op for an unknown
        session — the ``WHERE`` simply matches no row."""
        await self._pool.execute(
            """UPDATE conversation_sessions
                  SET metadata = COALESCE(metadata, '{}'::jsonb) || $2::jsonb, updated_at = now()
                WHERE session_id = $1""",
            session_id,
            json.dumps({"otpVerified": verified}),
        )

    # ── AdminStore ──────────────────────────────────────────────────────────

    async def list_connectors(self, org_id: str) -> list[dict[str, Any]]:
        """An org's connectors, sorted by name."""
        rows = await self._pool.fetch(
            """SELECT id, name, kind, config, enabled, created_at, updated_at
                 FROM connector_configs WHERE org_id = $1 ORDER BY name""",
            org_id,
        )
        return [self._to_connector(row, org_id) for row in rows]

    async def get_connector(self, org_id: str, connector_id: str) -> dict[str, Any] | None:
        """An org's connector, or ``None`` when unknown. A connector belonging to
        ANOTHER org returns ``None`` too — the caller renders the same 404, so the id
        space cannot be probed across orgs."""
        row = await self._pool.fetchrow(
            """SELECT id, name, kind, config, enabled, created_at, updated_at
                 FROM connector_configs WHERE org_id = $1 AND id = $2""",
            org_id,
            connector_id,
        )
        return self._to_connector(row, org_id) if row is not None else None

    @staticmethod
    def _to_connector(row: Any, org_id: str) -> dict[str, Any]:
        config = row["config"]
        return {
            "id": row["id"],
            "name": row["name"],
            "kind": row["kind"],
            "config": json.loads(config) if isinstance(config, str) else dict(config or {}),
            "enabled": row["enabled"],
            "createdAt": _iso(row["created_at"]),
            "updatedAt": _iso(row["updated_at"]),
            "_orgId": org_id,
        }

    async def put_connector(self, connector: dict[str, Any]) -> None:
        """Insert or update a connector in its org."""
        await self._pool.execute(
            """INSERT INTO connector_configs (org_id, id, name, kind, config, enabled, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $8)
               ON CONFLICT (org_id, id) DO UPDATE SET
                   name = EXCLUDED.name, kind = EXCLUDED.kind, config = EXCLUDED.config,
                   enabled = EXCLUDED.enabled, updated_at = EXCLUDED.updated_at""",
            connector["_orgId"],
            connector["id"],
            connector["name"],
            connector["kind"],
            json.dumps(connector.get("config") or {}),
            bool(connector.get("enabled")),
            datetime.fromisoformat(connector["createdAt"]),
            datetime.fromisoformat(connector["updatedAt"]),
        )

    async def delete_connector(self, org_id: str, connector_id: str) -> bool:
        """Remove an org's connector, reporting whether it existed. A cross-org id
        deletes nothing and reports ``False``."""
        tag = await self._pool.execute(
            "DELETE FROM connector_configs WHERE org_id = $1 AND id = $2",
            org_id,
            connector_id,
        )
        return tag != "DELETE 0"

    async def get_settings(self, org_id: str) -> dict[str, Any] | None:
        """An org's settings, or ``None`` when unset (the caller substitutes defaults)."""
        row = await self._pool.fetchrow(
            "SELECT model, system_prompt, default_tools, updated_at FROM agent_settings WHERE org_id = $1",
            org_id,
        )
        if row is None:
            return None
        tools = row["default_tools"]
        return {
            "orgId": org_id,
            "model": row["model"],
            "systemPrompt": row["system_prompt"],
            "defaultTools": json.loads(tools) if isinstance(tools, str) else list(tools or []),
            "updatedAt": _iso(row["updated_at"]),
        }

    async def put_settings(self, settings: dict[str, Any]) -> None:
        """Write an org's settings (one row per org)."""
        await self._pool.execute(
            """INSERT INTO agent_settings (org_id, model, system_prompt, default_tools, updated_at)
               VALUES ($1, $2, $3, $4::jsonb, $5)
               ON CONFLICT (org_id) DO UPDATE SET
                   model = EXCLUDED.model, system_prompt = EXCLUDED.system_prompt,
                   default_tools = EXCLUDED.default_tools, updated_at = EXCLUDED.updated_at""",
            settings["orgId"],
            settings["model"],
            settings.get("systemPrompt") or "",
            json.dumps(settings.get("defaultTools") or []),
            datetime.fromisoformat(settings["updatedAt"]),
        )

    async def list_runs(self, org_id: str) -> list[dict[str, Any]]:
        """An org's indexing runs, oldest first (insertion order, like the in-memory list)."""
        rows = await self._pool.fetch(
            """SELECT id, connector_name, status, started_at, finished_at, documents_seen,
                      chunks_indexed, documents_skipped, error
                 FROM indexing_runs WHERE org_id = $1 ORDER BY started_at, id""",
            org_id,
        )
        return [
            {
                "id": row["id"],
                "connectorName": row["connector_name"],
                "status": row["status"],
                "startedAt": _iso(row["started_at"]),
                "finishedAt": _iso(row["finished_at"]),
                "documentsSeen": int(row["documents_seen"]),
                "chunksIndexed": int(row["chunks_indexed"]),
                "documentsSkipped": int(row["documents_skipped"]),
                "error": row["error"],
                "_orgId": org_id,
            }
            for row in rows
        ]

    async def record_run(self, run: dict[str, Any]) -> None:
        """Insert or update an indexing run."""
        finished = run.get("finishedAt")
        await self._pool.execute(
            """INSERT INTO indexing_runs (id, org_id, connector_name, status, started_at, finished_at,
                                          documents_seen, chunks_indexed, documents_skipped, error)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
               ON CONFLICT (id) DO UPDATE SET
                   status = EXCLUDED.status, finished_at = EXCLUDED.finished_at,
                   documents_seen = EXCLUDED.documents_seen, chunks_indexed = EXCLUDED.chunks_indexed,
                   documents_skipped = EXCLUDED.documents_skipped, error = EXCLUDED.error""",
            run["id"],
            run["_orgId"],
            run["connectorName"],
            run["status"],
            datetime.fromisoformat(run["startedAt"]),
            datetime.fromisoformat(finished) if finished else None,
            int(run.get("documentsSeen") or 0),
            int(run.get("chunksIndexed") or 0),
            int(run.get("documentsSkipped") or 0),
            run.get("error"),
        )


async def resolve_storage(env: dict[str, str] | None = None) -> PostgresStore | None:
    """The storage backend named by ``SMOOTH_AGENT_STORAGE`` — the same contract the
    Rust server uses:

    - ``memory`` (or unset) → ``None``; the caller keeps its in-memory stores.
    - ``postgres`` → a :class:`PostgresStore` on ``SMOOTH_AGENT_DATABASE_URL``, falling
      back to ``DATABASE_URL`` but only once ``postgres`` has been asked for explicitly
      — an ambient ``DATABASE_URL`` alone can never change where data goes.

    Any other value raises rather than silently falling back to memory: a host that
    asked for durability and quietly got none is the failure worth shouting about.
    """
    source = os.environ if env is None else env
    backend = (source.get("SMOOTH_AGENT_STORAGE") or "").strip()
    if backend in ("", "memory"):
        return None
    if backend != "postgres":
        raise ValueError(f"unknown SMOOTH_AGENT_STORAGE {backend!r} (want memory or postgres)")
    dsn = (source.get("SMOOTH_AGENT_DATABASE_URL") or "").strip() or (source.get("DATABASE_URL") or "").strip()
    if not dsn:
        raise ValueError("SMOOTH_AGENT_STORAGE=postgres but neither SMOOTH_AGENT_DATABASE_URL nor DATABASE_URL is set")
    return await PostgresStore.create(dsn)
