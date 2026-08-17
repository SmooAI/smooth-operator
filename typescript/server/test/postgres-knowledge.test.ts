/**
 * Contract tests for the durable Postgres + pgvector knowledge store, against a REAL
 * Postgres in a throwaway container (testcontainers). The TS sibling of the C#
 * `KnowledgeBaseContractTests` / `AclKnowledgeContractTests` and the Rust adapter's
 * knowledge conformance suites.
 *
 * Same behavioural contract the reference impls assert:
 *   - ingest → retrieve the relevant document first; ingest is idempotent by id.
 *   - ACL leak boundary: anonymous → public only, entitled → private, unentitled → no leak.
 *
 * Docker is not required to run the rest of the suite: if a container cannot start,
 * every test here SKIPS (never fails). Unlike `postgres-store.test.ts` this needs the
 * pgvector image (`pgvector/pgvector:pg16`) for the `vector` type + HNSW index.
 *
 * Local gotcha (same as postgres-store.test.ts): on OrbStack the Ryuk reaper can hang;
 * `TESTCONTAINERS_RYUK_DISABLED=true pnpm test` gets past it.
 */

import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import type { AccessContext, Principal } from '../src/auth.js';
import { ANONYMOUS_ACCESS } from '../src/auth.js';
import { aclForGroups, DeterministicEmbedder, PostgresKnowledgeStore, PUBLIC_ACL } from '../src/postgresKnowledge.js';

const execFileAsync = promisify(execFile);

const DOCKER_PING_TIMEOUT_MS = 10_000;
const CONTAINER_UP_TIMEOUT_MS = 240_000;

/** Bound `work` with a deadline that holds even if `work` ignores it (see postgres-store.test.ts). */
function within<T>(limitMs: number, work: () => Promise<T>): Promise<T> {
    return Promise.race([
        work(),
        new Promise<never>((_resolve, reject) => {
            const timer = setTimeout(() => reject(new Error(`timed out after ${limitMs}ms`)), limitMs);
            timer.unref?.();
        }),
    ]);
}

let container: { stop: () => Promise<unknown> } | undefined;
let connectionString: string | undefined;
let skipReason: string | undefined;

beforeAll(async () => {
    try {
        await within(DOCKER_PING_TIMEOUT_MS, () => execFileAsync('docker', ['version', '--format', '{{.Server.Version}}']));
    } catch (error) {
        skipReason = `docker daemon not reachable: ${String(error)}`;
        return;
    }
    try {
        const { PostgreSqlContainer } = await import('@testcontainers/postgresql');
        // The pgvector image is a superset of postgres and provides the `vector` type.
        const started = await within(CONTAINER_UP_TIMEOUT_MS, () => new PostgreSqlContainer('pgvector/pgvector:pg16').start());
        container = started;
        connectionString = started.getConnectionUri();
    } catch (error) {
        skipReason = `could not start pgvector container: ${String(error)}`;
    }
}, CONTAINER_UP_TIMEOUT_MS + 30_000);

afterAll(async () => {
    await container?.stop();
});

/** A fresh store on the shared container, with the table cleared so tests don't bleed into each other. */
async function newStore(): Promise<PostgresKnowledgeStore> {
    if (!connectionString) throw new Error(skipReason ?? 'no container');
    const store = await PostgresKnowledgeStore.create(connectionString, new DeterministicEmbedder(256));
    // The table is shared across every store on this container; isolate each test.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any -- reach the pool for a one-off truncate
    await (store as any).pool.query('TRUNCATE knowledge_vectors');
    return store;
}

/** An access context for a caller in the given entitlement groups (anonymous when none). */
function withGroups(...groups: string[]): AccessContext {
    const principal: Principal = { sub: 'u', org: 'acme', role: 'basic', groups };
    return { principal, isAnonymous: groups.length === 0, authEnabled: true };
}

// The skip decision is made INSIDE the test body (beforeAll has not run at collection time).
const pgIt = (name: string, body: () => Promise<void>) =>
    it(name, async (ctx) => {
        if (!connectionString) ctx.skip(skipReason ?? 'no pgvector container');
        await body();
    });

describe('PostgresKnowledgeStore (needs Docker + pgvector)', () => {
    pgIt('ingest then query ranks the relevant doc first', async () => {
        const store = await newStore();
        try {
            await store.ingest({ id: 'returns', content: 'Our return window is 17 days from delivery.', source: 'returns.md' });
            await store.ingest({ id: 'shipping', content: 'Standard shipping takes 5 to 7 business days.', source: 'shipping.md' });

            const hits = await store.query('how long is the return window', 4);

            expect(hits.length).toBeGreaterThan(0);
            expect(hits[0].documentId).toBe('returns');
            expect(hits[0].chunk).toContain('17 days');
        } finally {
            await store.close();
        }
    });

    pgIt('ingest is idempotent by id', async () => {
        const store = await newStore();
        try {
            await store.ingest({ id: 'doc-x', content: 'original placeholder text', source: 'x.md' });
            await store.ingest({ id: 'doc-x', content: 'the refreshed payload mentions wombats', source: 'x.md' });

            const hits = await store.query('refreshed payload wombats', 4);

            const docX = hits.filter((h) => h.documentId === 'doc-x');
            expect(docX).toHaveLength(1); // a single row per id, not duplicated
            expect(docX[0].chunk).toContain('wombats');
        } finally {
            await store.close();
        }
    });

    pgIt('survives a new connection (durability)', async () => {
        const store = await newStore();
        await store.ingest({ id: 'persisted', content: 'The archived record endures across restarts.', source: 'p.md' });
        await store.close();

        // A brand-new store handle — nothing carried over in process memory.
        const reopened = await PostgresKnowledgeStore.create(connectionString!, new DeterministicEmbedder(256));
        try {
            const hits = await reopened.query('archived record endures restarts', 4);
            expect(hits.some((h) => h.documentId === 'persisted')).toBe(true);
        } finally {
            await reopened.close();
        }
    });

    // ── ACL leak contract (mirrors the C# AclKnowledgeContractTests) ────────────
    async function seeded(): Promise<PostgresKnowledgeStore> {
        const store = await newStore();
        await store.ingest({ id: 'pub', content: 'Public support hours are 9 to 5.', source: 'public.md' }, PUBLIC_ACL);
        await store.ingest(
            { id: 'secret', content: 'The private launch code is hunter2.', source: 'acme/private/launch.md' },
            aclForGroups('github:acme/private'),
        );
        return store;
    }

    pgIt('anonymous sees only public', async () => {
        const store = await seeded();
        try {
            const hits = await store.forAccess(ANONYMOUS_ACCESS).query('private launch code', 10);
            expect(hits.some((h) => h.documentId === 'secret')).toBe(false);
        } finally {
            await store.close();
        }
    });

    pgIt('entitled user reads the private doc', async () => {
        const store = await seeded();
        try {
            const hits = await store.forAccess(withGroups('github:acme/private')).query('private launch code', 10);
            expect(hits.some((h) => h.documentId === 'secret' && h.chunk.includes('hunter2'))).toBe(true);
        } finally {
            await store.close();
        }
    });

    pgIt('unentitled user gets no leak', async () => {
        const store = await seeded();
        try {
            const hits = await store.forAccess(withGroups('github:acme/other')).query('private launch code hunter2', 10);
            expect(hits.some((h) => h.documentId === 'secret')).toBe(false);
        } finally {
            await store.close();
        }
    });

    pgIt('withAcl stamps the ingest view', async () => {
        const store = await newStore();
        try {
            await store.withAcl(aclForGroups('github:acme/eng')).ingest({ id: 'eng', content: 'Engineering runbook: restart the widget.', source: 'eng.md' });
            const leaked = await store.forAccess(ANONYMOUS_ACCESS).query('engineering runbook restart widget', 10);
            expect(leaked.some((h) => h.documentId === 'eng')).toBe(false);
            const seen = await store.forAccess(withGroups('github:acme/eng')).query('engineering runbook restart widget', 10);
            expect(seen.some((h) => h.documentId === 'eng')).toBe(true);
        } finally {
            await store.close();
        }
    });
});

// A container-free guard: the deterministic embedder is stable and normalized, so the
// vector math the store relies on is exercised even when Docker is absent.
describe('DeterministicEmbedder', () => {
    it('is deterministic and L2-normalized', async () => {
        const embedder = new DeterministicEmbedder(64);
        const a = await embedder.embed('the quick brown fox');
        const b = await embedder.embed('the quick brown fox');
        expect(a).toEqual(b);
        expect(a).toHaveLength(64);
        const norm = Math.sqrt(a.reduce((sum, x) => sum + x * x, 0));
        expect(norm).toBeCloseTo(1, 5);
    });

    it('shares direction for texts that share tokens', async () => {
        const embedder = new DeterministicEmbedder(256);
        const returns = await embedder.embed('our return window is 17 days');
        const query = await embedder.embed('how long is the return window');
        const shipping = await embedder.embed('standard shipping takes business days');
        const cos = (x: number[], y: number[]) => x.reduce((s, xi, i) => s + xi * y[i], 0);
        // The query is closer to the doc that shares "return window" than to shipping.
        expect(cos(returns, query)).toBeGreaterThan(cos(shipping, query));
    });
});
