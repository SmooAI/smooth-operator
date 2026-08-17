"""Durable, vector-searched knowledge storage for the Python server — the sibling of
the Rust ``rust/adapters/postgres/src/knowledge.rs`` and the .NET
``PostgresKnowledgeBase`` / ``PostgresAclKnowledgeStore``.

Two stores, both on Postgres + ``pgvector``, both over the SAME ``knowledge_vectors``
table the Rust adapter defines (``rust/adapters/postgres/src/schema.rs``) so every
server in this repo shares one set of tables — same names, same columns:

- :class:`PostgresVectorKnowledge` — ingest documents, retrieve by cosine similarity.
  The durable analog of core's in-memory :class:`~smooth_operator_core.VectorKnowledge`.
- :class:`PostgresAclKnowledge` — the same, plus document-level access control. Each
  document carries an ACL (``{public, users, groups}``) persisted in the ``acl`` jsonb
  column; :meth:`~PostgresAclKnowledge.for_access` returns a read-only view that filters
  by the caller's entitlements **in SQL** — a restricted document is never even fetched.
  Mirrors Rust's ``PgKnowledgeBase::with_access`` and .NET's ``ForAccess``.

Retrieval reuses the offline, deterministic :class:`~smooth_operator_core.HashEmbedder`
by default (no network, stable across processes), sizing the ``vector(N)`` column to the
embedder's dimension. A gateway embedder drops in behind the same ``embed(text)`` seam.

``asyncpg`` is an OPTIONAL dependency (the ``postgres`` extra); importing this module is
what pulls it in. The embedding is stored by casting a pgvector text literal
(``$n::text::vector``) so no asyncpg type codec has to be registered — the same trick
the Rust adapter uses.
"""

from __future__ import annotations

import json
import uuid
from dataclasses import dataclass, field
from typing import Any, Optional, Protocol

import asyncpg
from smooth_operator_core import HashEmbedder, KnowledgeHit


class Embedder(Protocol):
    """Turns text into a fixed-length vector (core's :class:`~smooth_operator_core.Embedder`)."""

    def embed(self, text: str) -> list[float]: ...


# ── ACL value types ──────────────────────────────────────────────────────────
# Minimal mirrors of the Rust ``DocAcl`` / ``AccessContext`` and .NET ``DocumentAcl`` /
# ``AccessContext``. Core has no ACL types (the in-memory stores are ACL-free), so they
# live here with the durable store that needs them.


@dataclass(frozen=True)
class DocumentAcl:
    """A document's access control: public, or restricted to named users / groups.

    Serialized to the ``acl`` jsonb column as ``{public, users, groups}`` — the same
    shape Rust's ``DocAcl`` persists, so a row written here is readable cross-server."""

    public: bool = False
    users: tuple[str, ...] = ()
    groups: tuple[str, ...] = ()

    @classmethod
    def public_acl(cls) -> "DocumentAcl":
        return cls(public=True)

    @classmethod
    def for_groups(cls, *groups: str) -> "DocumentAcl":
        return cls(public=False, groups=tuple(groups))

    def to_json(self) -> str:
        return json.dumps({"public": self.public, "users": list(self.users), "groups": list(self.groups)})


@dataclass(frozen=True)
class AccessContext:
    """A requester's entitlements. ``anonymous`` carries no groups → public docs only."""

    user_id: Optional[str] = None
    groups: tuple[str, ...] = field(default=())
    anonymous: bool = True

    #: The anonymous requester — sees only public documents (fail-closed).
    @classmethod
    def anon(cls) -> "AccessContext":
        return cls(anonymous=True)

    @classmethod
    def for_groups(cls, *groups: str, user_id: str | None = None) -> "AccessContext":
        return cls(user_id=user_id, groups=tuple(groups), anonymous=len(groups) == 0)


def _vector_literal(vec: list[float]) -> str:
    """A pgvector text literal: ``[0.1,0.2,...]``. Cast ``::text::vector`` in SQL."""
    return "[" + ",".join(str(x) for x in vec) + "]"


# The ``knowledge_vectors`` DDL, verbatim in spirit from the Rust adapter's
# ``knowledge_vectors_schema(dim)`` so all servers share one table. The dimension is the
# active embedder's, so the column width always matches the vectors written into it.
def _schema(dim: int) -> str:
    return f"""
CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE IF NOT EXISTS knowledge_vectors (
    id              TEXT PRIMARY KEY,
    document_id     TEXT NOT NULL,
    organization_id TEXT,
    source          TEXT NOT NULL,
    content         TEXT NOT NULL,
    embedding       vector({dim}) NOT NULL,
    content_tsv     tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
    metadata        JSONB,
    acl             JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Idempotent add so a table created before the ACL column existed gains it in place.
ALTER TABLE knowledge_vectors ADD COLUMN IF NOT EXISTS acl JSONB;
CREATE INDEX IF NOT EXISTS idx_knowledge_content_tsv
    ON knowledge_vectors USING gin (content_tsv);
CREATE INDEX IF NOT EXISTS idx_knowledge_org
    ON knowledge_vectors (organization_id);
"""


class PostgresVectorKnowledge:
    """Durable vector knowledge on Postgres + pgvector. Cosine-ranked retrieval.

    Ingest is idempotent by document id (a document is one chunk keyed by its id, so
    re-ingesting the same id upserts in place). Retrieval embeds the query and ranks by
    pgvector cosine distance (``<=>``), returning core :class:`KnowledgeHit`\\ s so the
    result is protocol-compatible with the in-memory retrievers."""

    def __init__(self, pool: asyncpg.Pool, embedder: Embedder) -> None:
        self._pool = pool
        self._embedder = embedder

    @classmethod
    async def create(cls, dsn: str, embedder: Embedder | None = None) -> "PostgresVectorKnowledge":
        """Connect, create the pgvector extension + ``knowledge_vectors`` table, and
        return the store. The vector column is sized to the embedder's dimension,
        discovered by embedding a probe string so any :class:`Embedder` works."""
        embedder = embedder or HashEmbedder()
        dim = len(embedder.embed("dimension probe"))
        pool = await asyncpg.create_pool(dsn)
        assert pool is not None
        try:
            await pool.execute(_schema(dim))
        except BaseException:
            await pool.close()
            raise
        return cls(pool, embedder)

    async def close(self) -> None:
        await self._pool.close()

    async def ingest(
        self,
        content: str,
        source: str,
        *,
        document_id: str | None = None,
        organization_id: str | None = None,
        metadata: dict[str, Any] | None = None,
        acl: DocumentAcl | None = None,
    ) -> str:
        """Embed and upsert a document, returning its id. Idempotent by id: passing the
        same ``document_id`` again refreshes the row rather than duplicating it."""
        doc_id = document_id or str(uuid.uuid4())
        embedding = _vector_literal(self._embedder.embed(content))
        await self._pool.execute(
            """INSERT INTO knowledge_vectors
                   (id, document_id, organization_id, source, content, embedding, metadata, acl)
               VALUES ($1, $1, $2, $3, $4, $5::text::vector, $6::jsonb, $7::jsonb)
               ON CONFLICT (id) DO UPDATE SET
                   organization_id = EXCLUDED.organization_id,
                   source          = EXCLUDED.source,
                   content         = EXCLUDED.content,
                   embedding       = EXCLUDED.embedding,
                   metadata        = EXCLUDED.metadata,
                   acl             = EXCLUDED.acl""",
            doc_id,
            organization_id,
            source,
            content,
            embedding,
            json.dumps(metadata) if metadata is not None else None,
            acl.to_json() if acl is not None else None,
        )
        return doc_id

    async def query(self, query: str, top_k: int = 4, *, organization_id: str | None = None) -> list[KnowledgeHit]:
        """The ``top_k`` documents nearest the query by cosine similarity. ``score`` is
        ``1 - cosine_distance`` (higher is closer), matching the .NET store."""
        if top_k <= 0:
            return []
        rows = await self._query_rows(query, top_k, organization_id=organization_id, access=None)
        return [KnowledgeHit(content=r["content"], source=r["source"], score=float(r["score"])) for r in rows]

    async def _query_rows(
        self,
        query: str,
        top_k: int,
        *,
        organization_id: str | None,
        access: AccessContext | None,
    ) -> list[asyncpg.Record]:
        literal = _vector_literal(self._embedder.embed(query))
        # ACL predicate (only when this is an access-scoped read). A row is visible when
        # it has no recorded ACL (org-public default), is explicitly public, names the
        # requester's user id, or names any of the requester's groups — mirroring the
        # Rust ``query_async`` filter. ``?`` / ``?|`` are jsonb key-exists operators.
        if access is None:
            return await self._pool.fetch(
                """SELECT content, source, 1 - (embedding <=> $1::text::vector) AS score
                     FROM knowledge_vectors
                    WHERE ($2::text IS NULL OR organization_id = $2)
                    ORDER BY embedding <=> $1::text::vector
                    LIMIT $3""",
                literal,
                organization_id,
                top_k,
            )
        return await self._pool.fetch(
            """SELECT content, source, 1 - (embedding <=> $1::text::vector) AS score
                 FROM knowledge_vectors
                WHERE ($2::text IS NULL OR organization_id = $2)
                  AND (acl IS NULL
                       OR (acl->>'public')::boolean IS TRUE
                       OR ($4::text IS NOT NULL AND acl->'users' ? $4)
                       OR (acl->'groups' ?| $5::text[]))
                ORDER BY embedding <=> $1::text::vector
                LIMIT $3""",
            literal,
            organization_id,
            top_k,
            access.user_id,
            list(access.groups),
        )


class _ScopedView:
    """A read-only knowledge view bound to one requester's entitlements — the object
    returned by :meth:`PostgresAclKnowledge.for_access`. Its :meth:`query` filters by the
    ACL in SQL; it cannot ingest (mirrors .NET's ``ScopedView``)."""

    def __init__(self, store: "PostgresAclKnowledge", access: AccessContext) -> None:
        self._store = store
        self._access = access

    async def query(self, query: str, top_k: int = 4, *, organization_id: str | None = None) -> list[KnowledgeHit]:
        if top_k <= 0:
            return []
        rows = await self._store._backing._query_rows(
            query, top_k, organization_id=organization_id, access=self._access
        )
        return [KnowledgeHit(content=r["content"], source=r["source"], score=float(r["score"])) for r in rows]


class PostgresAclKnowledge:
    """Durable, ACL-aware vector knowledge. Ingest carries a :class:`DocumentAcl`;
    :meth:`for_access` returns a view whose queries a restricted document never reaches.
    Same ``knowledge_vectors`` table and cosine ranking as
    :class:`PostgresVectorKnowledge` — the ACL is one jsonb column and one SQL predicate.
    """

    def __init__(self, backing: PostgresVectorKnowledge) -> None:
        self._backing = backing

    @classmethod
    async def create(cls, dsn: str, embedder: Embedder | None = None) -> "PostgresAclKnowledge":
        return cls(await PostgresVectorKnowledge.create(dsn, embedder))

    async def close(self) -> None:
        await self._backing.close()

    async def ingest(
        self,
        content: str,
        source: str,
        acl: DocumentAcl,
        *,
        document_id: str | None = None,
        organization_id: str | None = None,
    ) -> str:
        return await self._backing.ingest(
            content, source, document_id=document_id, organization_id=organization_id, acl=acl
        )

    def for_access(self, access: AccessContext) -> _ScopedView:
        """A read-only view enforcing ``access``'s entitlements in SQL."""
        return _ScopedView(self, access)
