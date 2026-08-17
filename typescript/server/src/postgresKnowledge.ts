/**
 * Durable, vector-searched knowledge storage for the TypeScript server — the TS
 * sibling of the Rust `PgKnowledgeBase` (`rust/adapters/postgres/src/knowledge.rs`)
 * and the C# `PostgresKnowledgeBase` / `PostgresAclKnowledgeStore`
 * (`dotnet/server/postgres/src`).
 *
 * A document is embedded (via an injected {@link Embedder}) and stored as one row
 * in the shared `knowledge_vectors` table — the SAME table the Rust adapter creates
 * (`rust/adapters/postgres/src/schema.rs`), so a row written here reads back in every
 * other server in this repo. Retrieval embeds the query and ranks by pgvector cosine
 * distance (`embedding <=> $q`). Access control lives in the `acl` JSONB column and is
 * filtered IN SQL — a restricted document is never even fetched — exactly as the Rust
 * `query_async` ACL predicate does.
 *
 * ponytail: cosine-only dense retrieval. Rust additionally runs a sparse tsvector arm
 * and fuses the two with RRF; the shared behavioural contract (ingest → retrieve the
 * relevant doc, idempotent by id, ACL leak boundary) is satisfied by dense alone (the
 * C# reference is dense-only too). The `content_tsv` column still comes free with the
 * shared schema. Add the sparse arm + RRF here if lexical recall proves it earns its keep.
 */

import { Pool } from 'pg';

import type { AccessContext } from './auth.js';

/** A document to ingest. Mirrors the C# `KnowledgeDocument` / Rust `Document`. */
export interface KnowledgeDocument {
    id: string;
    content: string;
    source: string;
}

/** A retrieval hit. Mirrors the C# `KnowledgeResult` (documentId + chunk + score + source). */
export interface KnowledgeQueryResult {
    documentId: string;
    chunk: string;
    score: number;
    source: string;
}

/**
 * Document-level access control, persisted as the `acl` JSONB column. The serialized
 * shape (`{public, users, groups}`) is byte-compatible with the Rust `DocAcl`, so an
 * ACL written here filters correctly in the Rust adapter and vice-versa. NULL column
 * ⇒ no ACL recorded ⇒ org-public (the backward-compatible default).
 */
export interface DocumentAcl {
    public: boolean;
    users?: string[];
    groups?: string[];
}

/** A public document — visible to everyone. Mirrors C# `DocumentAcl.PublicAcl`. */
export const PUBLIC_ACL: DocumentAcl = { public: true };

/** An ACL restricting a document to the given entitlement groups. Mirrors `DocumentAcl.ForGroups`. */
export function aclForGroups(...groups: string[]): DocumentAcl {
    return { public: false, groups };
}

/**
 * Turns text into an embedding vector for similarity search. The seam mirrors the
 * Rust/C# `Embedder`: a real gateway embedder for production, a deterministic one for
 * tests / offline use.
 */
export interface Embedder {
    readonly dimensions: number;
    embed(text: string): Promise<number[]>;
}

/**
 * A deterministic, network-free embedder — hashed bag-of-words into a fixed-dimension,
 * L2-normalized vector. Same text → same vector; texts sharing tokens are close in
 * cosine space. The TS analog of the Rust/C# `DeterministicEmbedder`; ideal for tests
 * and small in-process corpora.
 */
export class DeterministicEmbedder implements Embedder {
    readonly dimensions: number;

    constructor(dimensions = 256) {
        if (dimensions <= 0) throw new RangeError('dimensions must be positive');
        this.dimensions = dimensions;
    }

    // eslint-disable-next-line @typescript-eslint/require-await -- async to satisfy the Embedder seam
    async embed(text: string): Promise<number[]> {
        const vector = new Array<number>(this.dimensions).fill(0);
        for (const token of tokenize(text)) {
            const slot = fnv1a(token) % this.dimensions;
            vector[slot] = (vector[slot] ?? 0) + 1;
        }
        // L2-normalize so cosine distance is well-behaved.
        let sumOfSquares = 0;
        for (const value of vector) sumOfSquares += value * value;
        if (sumOfSquares > 0) {
            const norm = Math.sqrt(sumOfSquares);
            for (let i = 0; i < vector.length; i++) vector[i] = (vector[i] ?? 0) / norm;
        }
        return vector;
    }
}

/** Lowercase, strip non-alphanumerics, keep tokens longer than 2 chars. Mirrors the C# tokenizer. */
function tokenize(text: string): string[] {
    const tokens: string[] = [];
    for (const raw of text.toLowerCase().split(/\s+/)) {
        const token = raw.replace(/[^a-z0-9]/g, '');
        if (token.length > 2) tokens.push(token);
    }
    return tokens;
}

/** 32-bit FNV-1a, unsigned. Same constants as the Rust/C# reference. */
function fnv1a(value: string): number {
    let hash = 2166136261;
    for (let i = 0; i < value.length; i++) {
        hash ^= value.charCodeAt(i);
        hash = Math.imul(hash, 16777619);
    }
    return hash >>> 0;
}

/** Format a vector as a pgvector literal: `[0.1,0.2,...]`. */
function vectorLiteral(v: number[]): string {
    return `[${v.join(',')}]`;
}

/**
 * The DDL applied on connect — the `knowledge_vectors` table verbatim from the Rust
 * adapter (`rust/adapters/postgres/src/schema.rs`, `knowledge_vectors_schema`), so the
 * table is byte-identical across every server. Requires a pgvector-enabled Postgres
 * (`pgvector/pgvector:pg16`). Every statement is idempotent.
 */
function schema(dim: number): string {
    return `
CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE IF NOT EXISTS knowledge_vectors (
    id              TEXT PRIMARY KEY,
    document_id     TEXT NOT NULL,
    organization_id TEXT,
    source          TEXT NOT NULL,
    content         TEXT NOT NULL,
    embedding       vector(${dim}) NOT NULL,
    content_tsv     tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
    metadata        JSONB,
    acl             JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE knowledge_vectors ADD COLUMN IF NOT EXISTS acl JSONB;
CREATE INDEX IF NOT EXISTS idx_knowledge_embedding_hnsw
    ON knowledge_vectors USING hnsw (embedding vector_cosine_ops);
CREATE INDEX IF NOT EXISTS idx_knowledge_content_tsv
    ON knowledge_vectors USING gin (content_tsv);
CREATE INDEX IF NOT EXISTS idx_knowledge_org
    ON knowledge_vectors (organization_id);
`;
}

/**
 * The ACL visibility predicate, applied in SQL to every query. A row is visible when it
 * has no recorded ACL (org-public), is explicitly public, names the requester's user id,
 * or names any of the requester's groups (`?` / `?|` are jsonb key-exists operators).
 * Verbatim from the Rust `query_async` predicate. `$2` is the requester user id (NULL ⇒
 * anonymous), `$3` the requester groups (text[]).
 */
const ACL_PREDICATE = `(acl IS NULL
    OR (acl->>'public')::boolean IS TRUE
    OR ($2::text IS NOT NULL AND acl->'users' ? $2)
    OR (acl->'groups' ?| $3::text[]))`;

/**
 * A durable, vector-searched, ACL-aware knowledge store on one Postgres + pgvector pool.
 *
 * Follows the Rust model of ONE type carrying optional access, rather than the C# split
 * into two classes: the plain retrieval path is {@link query} with no access (only
 * ACL-null/public rows match), and the chat path is {@link forAccess}, whose queries
 * filter rows by the requester's entitlements in SQL. {@link withAcl} mirrors the C#
 * ingest view for writing a document with an ACL.
 */
export class PostgresKnowledgeStore {
    private constructor(
        private readonly pool: Pool,
        private readonly embedder: Embedder,
    ) {}

    /** Connect, enable pgvector, and apply the shared schema (all idempotent). */
    static async create(connectionString: string, embedder: Embedder): Promise<PostgresKnowledgeStore> {
        const pool = new Pool({ connectionString });
        try {
            await pool.query(schema(embedder.dimensions));
        } catch (error) {
            await pool.end();
            throw error;
        }
        return new PostgresKnowledgeStore(pool, embedder);
    }

    /** Release the connection pool. */
    async close(): Promise<void> {
        await this.pool.end();
    }

    /**
     * Insert or update a document, embedding its content. Upsert is keyed by document id
     * (the doc is stored as a single chunk keyed by its id, so re-ingesting the same doc
     * replaces it in place — idempotent by id). `acl` NULL ⇒ org-public.
     */
    async ingest(doc: KnowledgeDocument, acl?: DocumentAcl): Promise<void> {
        const embedding = await this.embedder.embed(doc.content);
        await this.pool.query(
            `INSERT INTO knowledge_vectors (id, document_id, source, content, embedding, acl)
             VALUES ($1, $1, $2, $3, $4::text::vector, $5::jsonb)
             ON CONFLICT (id) DO UPDATE SET
                 document_id = EXCLUDED.document_id,
                 source      = EXCLUDED.source,
                 content     = EXCLUDED.content,
                 embedding   = EXCLUDED.embedding,
                 acl         = EXCLUDED.acl`,
            [doc.id, doc.source, doc.content, vectorLiteral(embedding), acl ? JSON.stringify(acl) : null],
        );
    }

    /**
     * The top-`limit` documents by cosine similarity to `query`. Without an access
     * context only ACL-null/public rows match (the plain-retrieval contract); with one,
     * rows are filtered by the requester's user id + groups in SQL before ranking, so a
     * restricted document is never fetched. `score` is cosine similarity (`1 - distance`).
     */
    async query(query: string, limit: number, access?: AccessContext): Promise<KnowledgeQueryResult[]> {
        const embedding = await this.embedder.embed(query);
        const userId = access && !access.isAnonymous ? access.principal.sub : null;
        const groups = access ? access.principal.groups : [];
        const { rows } = await this.pool.query(
            `SELECT document_id, content, source, 1 - (embedding <=> $1::text::vector) AS score
               FROM knowledge_vectors
              WHERE ${ACL_PREDICATE}
              ORDER BY embedding <=> $1::text::vector
              LIMIT $4`,
            [vectorLiteral(embedding), userId, groups, limit > 0 ? limit : 0],
        );
        return rows.map((row) => ({
            documentId: row.document_id as string,
            chunk: row.content as string,
            score: Number(row.score),
            source: row.source as string,
        }));
    }

    /**
     * A read-only view whose {@link KnowledgeView.query} enforces `access`'s
     * entitlements. Mirrors the Rust `with_access` / C# `ForAccess`.
     */
    forAccess(access: AccessContext): KnowledgeView {
        return { query: (q, limit) => this.query(q, limit, access) };
    }

    /**
     * A write view whose {@link IngestView.ingest} stamps every document with `acl`.
     * Mirrors the C# `WithAcl`.
     */
    withAcl(acl: DocumentAcl): IngestView {
        return { ingest: (doc) => this.ingest(doc, acl) };
    }
}

/** A read-only, access-scoped retrieval view. */
export interface KnowledgeView {
    query(query: string, limit: number): Promise<KnowledgeQueryResult[]>;
}

/** A write view that stamps a fixed ACL onto every ingested document. */
export interface IngestView {
    ingest(doc: KnowledgeDocument): Promise<void>;
}
