/**
 * Round-trip tests for the durable Postgres store, against a REAL Postgres in a
 * throwaway container (testcontainers) — the TS sibling of the Rust adapter's
 * `conformance.rs` / `admin_conformance.rs`, the Go `postgres_store_test.go`, and the
 * C# `PostgresSessionStoreTests`.
 *
 * Docker is not required to run the rest of the suite: if a container cannot start,
 * every Postgres test SKIPS. The "memory stays the default" tests need no container
 * at all — they are the guard that the in-memory path is untouched when
 * `SMOOTH_AGENT_STORAGE` is unset.
 *
 * Local gotcha: on OrbStack, testcontainers' Ryuk reaper can hang before the database
 * container is ever started, and these all skip on the timeout with Docker plainly
 * running. `TESTCONTAINERS_RYUK_DISABLED=true pnpm test` gets past it (at the cost of
 * leaving containers behind to clean up by hand). CI's plain dockerd runs Ryuk fine,
 * so this stays off by default.
 */

import { execFile } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { promisify } from 'node:util';
import { Pool } from 'pg';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { InMemoryAdminStore } from '../src/admin.js';
import { PostgresStore, resolveStorage } from '../src/postgresStore.js';
import { InMemorySessionStore } from '../src/sessionStore.js';
import { InMemorySessionStore } from '../src/sessionStore.js';

const execFileAsync = promisify(execFile);

/**
 * How long to wait for the Docker daemon to answer, and for a container to be serving.
 * The ping is short because its only job is to turn "no daemon" into a fast skip; the
 * start is generous because a cold machine pulls the image first.
 */
const DOCKER_PING_TIMEOUT_MS = 10_000;
const CONTAINER_UP_TIMEOUT_MS = 240_000;

/**
 * Run `work` with a deadline that holds even if `work` ignores it. Passing an abort
 * signal is not enough on its own: testcontainers resolves the Docker endpoint by
 * shelling out to the docker CLI, and against a dead daemon that subprocess can block
 * forever — which turns an intended skip into a hang the runner reports as a FAILURE.
 * A guard that fails open like that is worse than no guard, so the bound lives outside
 * the call rather than inside it.
 */
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
        // `docker version` is the probe rather than an API client because it asks the
        // SERVER (not just the client binary) and needs no extra dependency.
        await within(DOCKER_PING_TIMEOUT_MS, () => execFileAsync('docker', ['version', '--format', '{{.Server.Version}}']));
    } catch (error) {
        skipReason = `docker daemon not reachable: ${String(error)}`;
        return;
    }
    try {
        const { PostgreSqlContainer } = await import('@testcontainers/postgresql');
        const started = await within(CONTAINER_UP_TIMEOUT_MS, () => new PostgreSqlContainer('postgres:16-alpine').start());
        container = started;
        connectionString = started.getConnectionUri();
    } catch (error) {
        skipReason = `could not start postgres container: ${String(error)}`;
    }
}, CONTAINER_UP_TIMEOUT_MS + 30_000);

afterAll(async () => {
    await container?.stop();
});

/** A store on the shared container. Throws with the skip reason if there is none. */
async function newStore(): Promise<PostgresStore> {
    if (!connectionString) throw new Error(skipReason ?? 'no container');
    return PostgresStore.create(connectionString);
}

/** Every test namespaces its own org so they cannot see each other's rows. */
function org(prefix = 'org'): string {
    return `${prefix}-${randomUUID()}`;
}

describe('PostgresStore (needs Docker)', () => {
    /**
     * A test that needs the container. The skip decision is made INSIDE the test body,
     * not while the suite is being collected: `beforeAll` has not run at collection
     * time, so an `it` vs `it.skip` chosen there always sees "no container" and the
     * whole suite silently skips even on a machine with Docker running. That is the
     * failure mode where a green run means nothing was tested.
     */
    const pgIt = (name: string, body: () => Promise<void>) =>
        it(name, async (ctx) => {
            if (!connectionString) ctx.skip(skipReason ?? 'no postgres container');
            await body();
        });

    // The durability claim itself: a session and its messages written through one
    // store are readable through a SECOND store on the same database — i.e. they
    // survive the process that wrote them, which is the whole point of this backend.
    pgIt('survives a new connection', async () => {
        const store = await newStore();
        const orgId = org();

        const created = await store.createSession('', 'Alice', 'alice@example.test', undefined, orgId);
        expect(created.sessionId).toMatch(/^[0-9a-f-]{36}$/);
        await store.appendMessage(created.conversationId, 'inbound', 'hello');
        await store.appendMessage(created.conversationId, 'outbound', 'hi there');
        await store.close();

        // A brand-new store handle — nothing carried over in process memory.
        const reopened = await newStore();
        const fetched = await reopened.getSession(created.sessionId);
        expect(fetched).not.toBeNull();
        expect(fetched?.conversationId).toBe(created.conversationId);
        expect(fetched?.agentId).toBe(created.agentId);
        expect(fetched?.agentParticipantId).toBe(created.agentParticipantId);
        expect(fetched?.userEmail).toBe('alice@example.test');
        expect(fetched?.contactEmail).toBe('alice@example.test');

        const messages = await reopened.listMessages(created.conversationId, 50);
        expect(messages.map((m) => [m.direction, m.text])).toEqual([
            ['inbound', 'hello'],
            ['outbound', 'hi there'],
        ]);
        expect(messages[0]?.createdAt).toBeTruthy();

        expect(await reopened.getSession('does-not-exist')).toBeNull();
        await reopened.close();
    });

    // The most recent `limit`, oldest first — the in-memory contract.
    pgIt('lists the most recent messages oldest-first', async () => {
        const store = await newStore();
        const session = await store.createSession('', 'Alice', undefined, undefined, org());
        for (let i = 0; i < 5; i++) await store.appendMessage(session.conversationId, 'inbound', `m${i}`);

        expect((await store.listMessages(session.conversationId, 2)).map((m) => m.text)).toEqual(['m3', 'm4']);
        expect(await store.listMessages(session.conversationId, 0)).toHaveLength(5);
        await store.close();
    });

    // Resume binds the caller's OWN conversation; someone else's takes the identical
    // branch as an unknown id, so it cannot be used to probe which ids exist.
    pgIt('scopes resume by owner with no existence oracle', async () => {
        const store = await newStore();
        const orgId = org();

        const owned = await store.createSession('', 'Alice', 'alice@example.test', undefined, orgId);
        const resumed = await store.createSession('', 'Alice', 'alice@example.test', owned.conversationId, orgId);
        expect(resumed.conversationId).toBe(owned.conversationId);
        expect(resumed.userEmail).toBe('alice@example.test');

        // Bob names Alice's conversation …
        const bob = await store.createSession('', 'Bob', 'bob@example.test', owned.conversationId, orgId);
        expect(bob.conversationId).not.toBe(owned.conversationId);
        // … and gets exactly what he gets for an id that never existed.
        const unknown = await store.createSession('', 'Bob', 'bob@example.test', randomUUID(), orgId);
        expect(unknown.conversationId).not.toBe(owned.conversationId);

        // The resume must not have re-homed the conversation onto Bob.
        expect((await store.getSession(owned.sessionId))?.userEmail).toBe('alice@example.test');
        await store.close();
    });

    // An ownerless conversation (auth disabled, or an emailless principal) stays
    // reachable — denying it locks anonymous visitors out of what they just created.
    pgIt('keeps an ownerless conversation reachable', async () => {
        const store = await newStore();
        const orgId = org();

        const anonymous = await store.createSession('', '', undefined, undefined, orgId);
        expect(anonymous.userEmail).toBeUndefined();

        const carol = await store.createSession('', 'Carol', 'carol@example.test', anonymous.conversationId, orgId);
        expect(carol.conversationId).toBe(anonymous.conversationId);
        await store.close();
    });

    pgIt('scopes listConversations by owner, dropping empties', async () => {
        const store = await newStore();
        const orgId = org();

        const alice = await store.createSession('', 'Alice', 'alice@example.test', undefined, orgId);
        await store.appendMessage(alice.conversationId, 'inbound', 'alice asks');
        await store.appendMessage(alice.conversationId, 'outbound', 'agent answers');

        const bob = await store.createSession('', 'Bob', 'bob@example.test', undefined, orgId);
        await store.appendMessage(bob.conversationId, 'inbound', 'bob asks');

        // An empty conversation (every page-load mints one) must not show up.
        await store.createSession('', 'Alice', 'alice@example.test', undefined, orgId);

        const seen = await store.listConversations('alice@example.test', orgId);
        expect(seen).toHaveLength(1);
        expect(seen[0]?.conversationId).toBe(alice.conversationId);
        expect(seen[0]?.messageCount).toBe(2);
        expect(seen[0]?.firstInboundText).toBe('alice asks');
        expect(seen[0]?.updatedAt).toBeTruthy();

        const bobSees = await store.listConversations('bob@example.test', orgId);
        expect(bobSees.map((c) => c.conversationId)).toEqual([bob.conversationId]);
        await store.close();
    });

    // The auth-disabled flavor: undefined userEmail is unscoped by OWNER. It is the
    // path a laptop actually runs on, and it still must not cross orgs.
    pgIt('unscoped sees every conversation in its org but not another org', async () => {
        const store = await newStore();
        const orgId = org();

        for (const email of ['alice@example.test', undefined]) {
            const session = await store.createSession('', 'U', email, undefined, orgId);
            await store.appendMessage(session.conversationId, 'inbound', 'hi');
        }

        expect(await store.listConversations(undefined, orgId)).toHaveLength(2);
        expect(await store.listConversations(undefined, org('other'))).toHaveLength(0);
        await store.close();
    });

    // Org is the OUTER scope. Driven with the SAME email in two orgs, so only the org
    // can be doing the isolating.
    pgIt('isolates organizations', async () => {
        const store = await newStore();
        const orgA = org('a');
        const orgB = org('b');

        const inA = await store.createSession('', 'Shared', 'shared@example.test', undefined, orgA);
        await store.appendMessage(inA.conversationId, 'inbound', 'org A only');

        expect(await store.listConversations('shared@example.test', orgB)).toHaveLength(0);
        expect(await store.getConversation(inA.conversationId, orgB)).toBeNull();

        const crossOrg = await store.createSession('', 'Shared', 'shared@example.test', inA.conversationId, orgB);
        expect(crossOrg.conversationId).not.toBe(inA.conversationId);
        await store.close();
    });

    pgIt('persists the workflow step and the OTP bit', async () => {
        const store = await newStore();
        const session = await store.createSession('', 'Alice', 'alice@example.test', undefined, org());
        await store.setCurrentStep(session.sessionId, 'collect-email');
        await store.setAuthenticated(session.sessionId, true);
        await store.close();

        const reopened = await newStore();
        const fetched = await reopened.getSession(session.sessionId);
        expect(fetched?.currentStepId).toBe('collect-email');
        expect(fetched?.otpVerified).toBe(true);
        // The second write must not have clobbered the first, nor the contact email.
        expect(fetched?.contactEmail).toBe('alice@example.test');

        // No-ops for an unknown session, never errors.
        await expect(reopened.setCurrentStep('unknown-session', 'whatever')).resolves.toBeUndefined();
        await expect(reopened.setAuthenticated('unknown-session', true)).resolves.toBeUndefined();
        await reopened.close();
    });

    pgIt('attachIdentity stamps the contact keys the OTP seam reads, without clobbering siblings', async () => {
        const store = await newStore();
        const session = await store.createSession('', 'Alice', undefined, undefined, org());
        await store.setAuthenticated(session.sessionId, true);
        // The identity_intake host effect: only the provided fields are stamped.
        await store.attachIdentity(session.sessionId, { name: 'Alice Example', email: 'alice@example.com', phone: '+15551234567' });
        await store.close();

        const reopened = await newStore();
        const fetched = await reopened.getSession(session.sessionId);
        expect(fetched?.userName).toBe('Alice Example');
        expect(fetched?.contactEmail).toBe('alice@example.com');
        expect(fetched?.contactPhone).toBe('+15551234567');
        // The prior OTP bit survived the metadata merge.
        expect(fetched?.otpVerified).toBe(true);

        await expect(reopened.attachIdentity('unknown-session', { email: 'x@y.co' })).resolves.toBeUndefined();
        await reopened.close();
    });

    // The durable store must report the conversation's ORG on the session and on
    // getConversation, not just the owner. `mayRead` treats an absent org as
    // "unrecorded" and falls through to an ownership-only check — so a store that drops
    // it reopens the cross-org hole for ownerless conversations while every existing
    // test still passes. Uses an OWNERLESS conversation: an owned one would pass for
    // the wrong reason, because ownership alone would block the cross-org read.
    pgIt('reports orgId so the dispatcher gate can enforce it', async () => {
        const store = await newStore();
        const orgA = org('a');

        const anonymous = await store.createSession('', '', undefined, undefined, orgA);
        expect(anonymous.userEmail).toBeUndefined();
        expect(anonymous.orgId).toBe(orgA);

        const fetched = await store.getSession(anonymous.sessionId);
        expect(fetched?.orgId).toBe(orgA);

        const conv = await store.getConversation(anonymous.conversationId, orgA);
        expect(conv?.orgId).toBe(orgA);
        await store.close();
    });

    pgIt('scopes connectors by org', async () => {
        const store = await newStore();
        const orgA = org('a');
        const orgB = org('b');
        const now = new Date().toISOString();

        const zendesk = { id: randomUUID(), name: 'zendesk', kind: 'helpdesk', config: { subdomain: 'acme' }, enabled: true, createdAt: now, updatedAt: now, orgId: orgA };
        await store.putConnector(zendesk);
        await store.putConnector({ id: randomUUID(), name: 'algolia', kind: 'search', config: {}, enabled: false, createdAt: now, updatedAt: now, orgId: orgA });
        await store.close();

        // Read back through a fresh connection — durability, not a process-local map.
        const reopened = await newStore();
        const list = await reopened.listConnectors(orgA);
        expect(list.map((c) => c.name)).toEqual(['algolia', 'zendesk']);
        expect(list[1]?.config).toEqual({ subdomain: 'acme' });
        expect(list[1]?.enabled).toBe(true);

        // Org B sees nothing; a cross-org id reports exactly like an unknown one.
        expect(await reopened.listConnectors(orgB)).toHaveLength(0);
        expect(await reopened.getConnector(orgB, zendesk.id)).toBeUndefined();
        expect(await reopened.getConnector(orgB, randomUUID())).toBeUndefined();
        expect(await reopened.deleteConnector(orgB, zendesk.id)).toBe(false);

        // Upsert updates in place rather than duplicating.
        await reopened.putConnector({ ...zendesk, name: 'zendesk-eu', enabled: false });
        const updated = await reopened.getConnector(orgA, zendesk.id);
        expect(updated?.name).toBe('zendesk-eu');
        expect(updated?.enabled).toBe(false);
        expect(await reopened.listConnectors(orgA)).toHaveLength(2);

        expect(await reopened.deleteConnector(orgA, zendesk.id)).toBe(true);
        expect(await reopened.getConnector(orgA, zendesk.id)).toBeUndefined();
        await reopened.close();
    });

    pgIt('round-trips settings, one row per org', async () => {
        const store = await newStore();
        const orgId = org();

        // An unset org reports undefined so the handler can substitute defaults.
        expect(await store.getSettings(orgId)).toBeUndefined();

        const written = { orgId, model: 'claude-haiku-4-5', systemPrompt: 'be brief', defaultTools: ['search', 'email'], updatedAt: new Date().toISOString() };
        await store.putSettings(written);
        await store.close();

        const reopened = await newStore();
        const read = await reopened.getSettings(orgId);
        expect(read?.model).toBe(written.model);
        expect(read?.systemPrompt).toBe(written.systemPrompt);
        expect(read?.defaultTools).toEqual(['search', 'email']);

        // A second put replaces rather than duplicating.
        await reopened.putSettings({ ...written, model: 'claude-sonnet-5' });
        expect((await reopened.getSettings(orgId))?.model).toBe('claude-sonnet-5');

        expect(await reopened.getSettings(org())).toBeUndefined();
        await reopened.close();
    });

    pgIt('scopes indexing runs by org', async () => {
        const store = await newStore();
        const orgA = org('a');
        const orgB = org('b');
        const startedAt = new Date().toISOString();
        const finishedAt = new Date(Date.parse(startedAt) + 1000).toISOString();

        const run = {
            id: randomUUID(),
            connectorName: 'zendesk',
            status: 'succeeded',
            startedAt,
            finishedAt,
            documentsSeen: 7,
            chunksIndexed: 21,
            documentsSkipped: 1,
            error: null,
            orgId: orgA,
        };
        await store.recordRun(run);
        await store.recordRun({ ...run, id: randomUUID(), connectorName: 'algolia', status: 'failed', orgId: orgB });
        await store.close();

        const reopened = await newStore();
        const runs = await reopened.listRuns(orgA);
        expect(runs).toHaveLength(1);
        expect(runs[0]).toMatchObject({
            id: run.id,
            connectorName: 'zendesk',
            status: 'succeeded',
            documentsSeen: 7,
            chunksIndexed: 21,
            documentsSkipped: 1,
            error: null,
        });
        expect(Date.parse(runs[0]!.finishedAt!)).toBe(Date.parse(finishedAt));

        // Re-recording the same id updates in place.
        await reopened.recordRun({ ...run, status: 'failed', error: 'boom' });
        const after = await reopened.listRuns(orgA);
        expect(after).toHaveLength(1);
        expect(after[0]?.status).toBe('failed');
        expect(after[0]?.error).toBe('boom');
        await reopened.close();
    });

    // ── schema integrity (th-5a5181 P2) ─────────────────────────────────────
    //
    // The json columns are NOT NULL DEFAULT '{}', so "absent" has ONE representation
    // on read instead of two.
    //
    // These inserts OMIT the json columns rather than passing an explicit NULL, so the
    // DEFAULT fires on its own — no coalesce needed here, unlike the Rust adapter whose
    // inserts name every column. This is what fails if either half regresses: drop the
    // NOT NULL DEFAULT and these read back null; start passing an explicit NULL and the
    // insert dies on the not-null constraint.
    pgIt('reads absent json back as an empty object', async () => {
        const store = await newStore();
        const orgId = org();
        const created = await store.createSession('', 'Alice', undefined, undefined, orgId);
        await store.appendMessage(created.conversationId, 'inbound', 'hello');

        const pool = new Pool({ connectionString });
        try {
            const conv = await pool.query('SELECT metadata_json, analytics_json FROM conversations WHERE id = $1', [created.conversationId]);
            expect(conv.rows[0].metadata_json).toEqual({});
            expect(conv.rows[0].analytics_json).toEqual({});

            const msg = await pool.query('SELECT metadata_json, analytics_json FROM conversation_messages WHERE conversation_id = $1', [
                created.conversationId,
            ]);
            expect(msg.rows[0].metadata_json).toEqual({});
            expect(msg.rows[0].analytics_json).toEqual({});

            const part = await pool.query('SELECT metadata_json FROM conversation_participants WHERE conversation_id = $1 LIMIT 1', [
                created.conversationId,
            ]);
            expect(part.rows[0].metadata_json).toEqual({});

            const sess = await pool.query(
                'SELECT status, created_at, updated_at, last_activity_at FROM conversation_sessions WHERE session_id = $1',
                [created.sessionId],
            );
            expect(sess.rows[0].status).toBe('active'); // passes the new CHECK
            expect(sess.rows[0].created_at).not.toBeNull();
            expect(sess.rows[0].updated_at).not.toBeNull();
            expect(sess.rows[0].last_activity_at).not.toBeNull();
        } finally {
            await pool.end();
            await store.close();
        }
    });

    // The CHECK is what stops a typo'd platform reaching the table at all.
    pgIt('rejects an unknown platform', async () => {
        const store = await newStore();
        const pool = new Pool({ connectionString });
        try {
            await expect(
                pool.query(
                    `INSERT INTO conversations (id, platform, name, organization_id, idempotency_key)
                     VALUES ($1, 'carrier-pigeon', '', $2, $1)`,
                    [randomUUID(), org()],
                ),
            ).rejects.toThrow(/conversations_platform_check/);
        } finally {
            await pool.end();
            await store.close();
        }
    });


    // ── agentless sessions (th-68897a) ──────────────────────────────────────
    //
    // A session created with no agentId has NO agent. Both stores used to mint a fresh
    // UUID here, which pointed every agentless session at an agent that never existed —
    // invisible until something tried to resolve it. Covers BOTH stores: the fabrication
    // lived in each, so testing one would leave the other broken.
    pgIt('a session with no agent has no agent', async () => {
        const memory = new InMemorySessionStore();
        for (const blank of ['', '   ']) {
            const m = await memory.createSession(blank, 'Alice');
            expect(m.agentId, `in-memory agentId for ${JSON.stringify(blank)}`).toBeUndefined();
        }

        const store = await newStore();
        try {
            const created = await store.createSession('   ', 'Alice', undefined, undefined, org());
            expect(created.agentId).toBeUndefined();

            // The column itself is NULL, not an empty string standing in for one.
            const pool = new Pool({ connectionString });
            try {
                const row = await pool.query('SELECT agent_id FROM conversation_sessions WHERE session_id = $1', [created.sessionId]);
                expect(row.rows[0].agent_id).toBeNull();
            } finally {
                await pool.end();
            }

            // …and it survives the round trip rather than coming back as a uuid.
            const fetched = await store.getSession(created.sessionId);
            expect(fetched?.agentId).toBeUndefined();
        } finally {
            await store.close();
        }
    });

});

describe('memory stays the default', () => {
    // The guard on the whole swap: with SMOOTH_AGENT_STORAGE unset (or memory) nothing
    // durable is resolved. Needs no Docker.
    it('resolves no storage when unset or memory', async () => {
        expect(await resolveStorage({})).toBeUndefined();
        expect(await resolveStorage({ SMOOTH_AGENT_STORAGE: 'memory' })).toBeUndefined();
        // An ambient DATABASE_URL alone must never switch the backend.
        expect(await resolveStorage({ DATABASE_URL: 'postgres://nope/nope' })).toBeUndefined();
    });

    // A durable backend that cannot be configured is fatal, never a silent fall back
    // to memory — losing durability quietly is the failure worth shouting about.
    it('rejects a misconfigured durable backend', async () => {
        await expect(resolveStorage({ SMOOTH_AGENT_STORAGE: 'postgres' })).rejects.toThrow(/neither SMOOTH_AGENT_DATABASE_URL nor DATABASE_URL/);
        await expect(resolveStorage({ SMOOTH_AGENT_STORAGE: 'cassandra' })).rejects.toThrow(/unknown SMOOTH_AGENT_STORAGE/);
    });

    // The in-memory stores keep behaving exactly as they did: the added orgId argument
    // is accepted and ignored, so a single-tenant caller sees no change at all.
    it('leaves the in-memory stores single-tenant and unchanged', async () => {
        const store = new InMemorySessionStore();
        const session = await store.createSession('agent-1', 'Alice', 'alice@example.test');
        await store.appendMessage(session.conversationId, 'inbound', 'hello');

        // Passing an org changes nothing for the memory store.
        expect(await store.listConversations('alice@example.test', 'some-org')).toHaveLength(1);
        expect(await store.listConversations('alice@example.test')).toHaveLength(1);
        expect(await store.getConversation(session.conversationId, 'another-org')).not.toBeNull();

        const admin = new InMemoryAdminStore();
        expect(await admin.listConnectors('public')).toEqual([]);
        expect(await admin.getSettings('public')).toBeUndefined();
    });
});
