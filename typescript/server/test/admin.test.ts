/**
 * The `/admin/*` API the console drives. Two things matter per route: it must
 * fail CLOSED without a sufficient token, and it must answer the wire shape the
 * console's typed client expects (camelCase, `{error:{code,message}}`).
 *
 * Driven over real HTTP against a booted server, so the routing, the auth gate
 * and the JSON all have to actually work together.
 */
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { ANONYMOUS_PRINCIPAL, type AccessContext, type AuthVerifier } from '../src/auth.js';
import { InMemoryBackplane } from '../src/backplane.js';
import { InMemorySessionStore } from '../src/sessionStore.js';
import { serve, type RunningServer } from '../src/server.js';
import { mapModelInfo, resetModelCostsCache } from '../src/admin.js';

/** An auth-enabled verifier that maps a token straight to a role. */
class RoleVerifier implements AuthVerifier {
    readonly mode = 'test';
    resolve(token: string | undefined): AccessContext {
        const principal = (sub: string, org: string, role: string) => ({
            principal: { sub, org, role, groups: [] },
            isAnonymous: false,
            authEnabled: true,
        });
        switch (token) {
            case 'admin':
            case 'curator':
            case 'basic':
                return principal(`u-${token}`, 'org-1', token);
            case 'other-org-admin':
                return principal('u-other', 'org-2', 'admin');
            default:
                return { principal: ANONYMOUS_PRINCIPAL, isAnonymous: true, authEnabled: true };
        }
    }
}

let authed: RunningServer;
let devMode: RunningServer;

beforeAll(async () => {
    const chatClient = { chat: { completions: { create: async () => ({ choices: [{ message: { content: '' } }] }) } } } as never;
    authed = await serve({ port: 0, chatClient, store: new InMemorySessionStore(), auth: new RoleVerifier() });
    // No auth verifier passed → the server's default NoAuthVerifier, mode 'none'.
    devMode = await serve({ port: 0, chatClient, store: new InMemorySessionStore() });
});
afterAll(async () => {
    await authed.close();
    await devMode.close();
});

function base(server: RunningServer): string {
    return `http://127.0.0.1:${server.port}`;
}

async function call(server: RunningServer, method: string, path: string, token?: string, body?: unknown) {
    // fetch refuses a body on GET/HEAD, so only writes carry one.
    if (method === 'GET' || method === 'HEAD') body = undefined;
    const res = await fetch(`${base(server)}${path}`, {
        method,
        headers: {
            ...(token ? { authorization: `Bearer ${token}` } : {}),
            ...(body !== undefined ? { 'content-type': 'application/json' } : {}),
        },
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    });
    const text = await res.text();
    return { status: res.status, json: text ? JSON.parse(text) : undefined };
}

/** Every gated route with the minimum role it requires — the contract table. */
const gatedRoutes: Array<[string, string]> = [
    ['GET', '/admin/me'],
    ['GET', '/admin/conversations'],
    ['GET', '/admin/conversations/c1/messages'],
    ['GET', '/admin/indexing/runs'],
    ['GET', '/admin/document-sets'],
    ['GET', '/admin/connectors'],
    ['POST', '/admin/connectors'],
    ['GET', '/admin/connectors/x'],
    ['PUT', '/admin/connectors/x'],
    ['DELETE', '/admin/connectors/x'],
    ['POST', '/admin/connectors/x/index'],
    ['GET', '/admin/settings'],
    ['PUT', '/admin/settings'],
];

describe('auth gate', () => {
    it('fails closed on every gated route without a token', async () => {
        for (const [method, path] of gatedRoutes) {
            const { status, json } = await call(authed, method, path, undefined, {});
            expect(status, `${method} ${path}`).toBe(401);
            expect(json.error.code, `${method} ${path}`).toBe('UNAUTHENTICATED');
        }
    });

    it('rejects an invalid token', async () => {
        const { status, json } = await call(authed, 'GET', '/admin/me', 'garbage');
        expect(status).toBe(401);
        expect(json.error.code).toBe('INVALID_TOKEN');
    });

    it('enforces role rank in both directions', async () => {
        expect((await call(authed, 'GET', '/admin/me', 'basic')).status).toBe(200);
        expect((await call(authed, 'GET', '/admin/settings', 'basic')).status).toBe(403);
        expect((await call(authed, 'GET', '/admin/settings', 'curator')).status).toBe(200);

        const denied = await call(authed, 'PUT', '/admin/settings', 'curator', { model: 'm' });
        expect(denied.status).toBe(403);
        expect(denied.json.error.code).toBe('FORBIDDEN');
    });

    it('leaves /admin/health ungated', async () => {
        const { status } = await call(authed, 'GET', '/admin/health');
        expect(status).toBe(200);
    });
});

describe('AUTH_MODE=none dev grant', () => {
    // Rust's NoAuthVerifier returns a fixed Admin principal there. Without the same
    // grant the console 403-walls against a local server — as useless as the 404s.
    it('grants admin on a no-auth server', async () => {
        for (const path of ['/admin/settings', '/admin/connectors', '/admin/indexing/runs']) {
            expect((await call(devMode, 'GET', path, 'dev-token')).status, path).toBe(200);
        }
    });

    it('still fails closed with no token at all', async () => {
        expect((await call(devMode, 'GET', '/admin/settings')).status).toBe(401);
    });

    it('does not leak into an auth-enabled server', async () => {
        expect((await call(authed, 'GET', '/admin/settings', 'basic')).status).toBe(403);
    });
});

describe('shapes the console consumes', () => {
    it('/admin/me returns the principal', async () => {
        const { json } = await call(authed, 'GET', '/admin/me', 'curator');
        expect(json).toMatchObject({ userId: 'u-curator', orgId: 'org-1', role: 'curator' });
    });

    it('conversations and messages carry their envelopes', async () => {
        const list = await call(authed, 'GET', '/admin/conversations', 'curator');
        expect(Array.isArray(list.json.conversations)).toBe(true);
        expect(list.json).toHaveProperty('nextCursor');

        const msgs = await call(authed, 'GET', '/admin/conversations/c1/messages', 'curator');
        expect(msgs.json.conversationId).toBe('c1');
        expect(Array.isArray(msgs.json.messages)).toBe(true);
    });

    it('document-sets answers with an empty list', async () => {
        const { json } = await call(authed, 'GET', '/admin/document-sets', 'curator');
        expect(json.documentSets).toEqual([]);
    });
});

describe('connector CRUD', () => {
    it('round-trips create → list → get → update → index → delete', async () => {
        const created = await call(authed, 'POST', '/admin/connectors', 'admin', {
            name: 'docs',
            kind: 'web',
            config: { url: 'https://x' },
            enabled: true,
        });
        expect(created.status).toBe(200);
        const id = created.json.connector.id;
        expect(id).toBeTruthy();
        expect(created.json.connector).toMatchObject({ name: 'docs', kind: 'web', enabled: true });
        expect(created.json.connector.createdAt).toBeTruthy();
        // The internal owner key must never reach the wire.
        expect(created.json.connector).not.toHaveProperty('orgId');

        const list = await call(authed, 'GET', '/admin/connectors', 'curator');
        expect(list.json.connectors).toHaveLength(1);

        const got = await call(authed, 'GET', `/admin/connectors/${id}`, 'curator');
        expect(got.json.connector.id).toBe(id);

        const updated = await call(authed, 'PUT', `/admin/connectors/${id}`, 'admin', {
            name: 'docs2',
            kind: 'web',
            config: {},
            enabled: false,
        });
        expect(updated.json.connector).toMatchObject({ name: 'docs2', enabled: false });

        const indexed = await call(authed, 'POST', `/admin/connectors/${id}/index`, 'curator');
        expect(indexed.status).toBe(200);
        const runs = await call(authed, 'GET', '/admin/indexing/runs', 'curator');
        expect(runs.json.runs).toHaveLength(1);

        expect((await call(authed, 'DELETE', `/admin/connectors/${id}`, 'admin')).status).toBe(204);
        expect((await call(authed, 'GET', `/admin/connectors/${id}`, 'curator')).status).toBe(404);
    });

    it('is org isolated — a foreign id is indistinguishable from an unknown one', async () => {
        const created = await call(authed, 'POST', '/admin/connectors', 'admin', {
            name: 'mine',
            kind: 'web',
            config: {},
            enabled: true,
        });
        const id = created.json.connector.id;

        expect((await call(authed, 'GET', `/admin/connectors/${id}`, 'other-org-admin')).status).toBe(404);
        const foreignList = await call(authed, 'GET', '/admin/connectors', 'other-org-admin');
        expect(foreignList.json.connectors).toHaveLength(0);
    });

    it('validates the write body', async () => {
        expect((await call(authed, 'POST', '/admin/connectors', 'admin', { kind: 'web' })).status).toBe(400);
    });
});

describe('settings', () => {
    it('reads defaults when unset, then round-trips a write', async () => {
        const initial = await call(authed, 'GET', '/admin/settings', 'curator');
        expect(initial.json.settings.orgId).toBe('org-1');
        expect(initial.json.settings.model).toBeTruthy();

        const put = await call(authed, 'PUT', '/admin/settings', 'admin', {
            model: 'claude-sonnet-4-5',
            systemPrompt: 'be nice',
            defaultTools: ['search'],
        });
        expect(put.json.settings).toMatchObject({ model: 'claude-sonnet-4-5', systemPrompt: 'be nice' });

        const reread = await call(authed, 'GET', '/admin/settings', 'curator');
        expect(reread.json.settings.model).toBe('claude-sonnet-4-5');
    });

    it('rejects a write with no model', async () => {
        expect((await call(authed, 'PUT', '/admin/settings', 'admin', { systemPrompt: 'x' })).status).toBe(400);
    });
});

describe('model costs', () => {
    // A representative /v1/model/info payload from the LiteLLM gateway.
    const payload = {
        data: [
            {
                model_name: 'claude-opus-4-8',
                model_info: {
                    input_cost_per_token: 0.000015,
                    output_cost_per_token: 0.000075,
                    model_tier: 'frontier',
                    use_cases: ['reasoning', 'coding'],
                    max_output_tokens: 65536,
                },
            },
            { model_name: 'mystery-model', model_info: {} },
            { model_info: { input_cost_per_token: 1 } }, // no model_name → skipped
        ],
    };

    it('maps the gateway payload to the console shape', () => {
        const got = mapModelInfo(payload);
        expect(Object.keys(got)).toEqual(['claude-opus-4-8', 'mystery-model']);
        expect(got['claude-opus-4-8']).toEqual({
            inputCostPerToken: 0.000015,
            outputCostPerToken: 0.000075,
            tier: 'frontier',
            useCases: ['reasoning', 'coding'],
            maxOutputTokens: 65536,
        });
    });

    it('leaves omitted fields null rather than defaulting them', () => {
        // A $0 default would render a free-model badge on a paid model.
        expect(mapModelInfo(payload)['mystery-model']).toEqual({
            inputCostPerToken: null,
            outputCostPerToken: null,
            tier: null,
            useCases: [],
            maxOutputTokens: null,
        });
    });

    it('tolerates garbage payloads', () => {
        expect(mapModelInfo({})).toEqual({});
        expect(mapModelInfo({ data: 'not-an-array' })).toEqual({});
        expect(mapModelInfo(undefined)).toEqual({});
    });

    it('is ungated and degrades to {} when the gateway is unreachable', async () => {
        resetModelCostsCache();
        const prev = process.env.SMOOAI_GATEWAY_URL;
        process.env.SMOOAI_GATEWAY_URL = 'http://127.0.0.1:1';
        try {
            // No bearer token at all — ungated.
            const { status, json } = await call(authed, 'GET', '/admin/model-costs');
            expect(status).toBe(200);
            expect(json).toEqual({});
            // A failure must NOT be cached, or one blip pins {} for the process.
            const again = await call(authed, 'GET', '/admin/model-costs');
            expect(again.status).toBe(200);
        } finally {
            process.env.SMOOAI_GATEWAY_URL = prev;
            resetModelCostsCache();
        }
    });
});

describe('realtime publish', () => {
    it('delivers to an attached connection and reports a truthful count', async () => {
        // serve() owns its backplane, so drive the handler's contract through a
        // server whose backplane we can attach to.
        const bp = new InMemoryBackplane();
        const seen: unknown[] = [];
        await bp.attach('conn-1', (frame) => seen.push(frame));
        const chatClient = { chat: { completions: { create: async () => ({ choices: [{ message: { content: '' } }] }) } } } as never;
        const server = await serve({ port: 0, chatClient, store: new InMemorySessionStore(), auth: new RoleVerifier(), backplane: bp });
        try {
            const { status, json } = await call(server, 'POST', '/admin/publish', 'admin', {
                target: { type: 'connection', id: 'conn-1' },
                event: { kind: 'job.done' },
            });
            expect(status).toBe(200);
            expect(json.delivered).toBe(1);
            expect(seen).toHaveLength(1);
        } finally {
            await server.close();
        }
    });

    it('reports a truthful 0 for a routable-but-unattached connection', async () => {
        const { status, json } = await call(authed, 'POST', '/admin/publish', 'admin', {
            target: { type: 'connection', id: 'nobody' },
            event: {},
        });
        expect(status).toBe(200);
        expect(json.delivered).toBe(0);
    });

    it('refuses targets the backplane cannot route, rather than reporting 0', async () => {
        // {"delivered": 0} would read as "accepted, reached nobody" for an event
        // that was never routable here. 501 says so out loud.
        for (const kind of ['session', 'user', 'org', 'agent']) {
            const { status, json } = await call(authed, 'POST', '/admin/publish', 'admin', {
                target: { type: kind, id: 'x' },
                event: {},
            });
            expect(status, kind).toBe(501);
            expect(json.error.code, kind).toBe('UNSUPPORTED_TARGET');
            expect(json, kind).not.toHaveProperty('delivered');
        }
    });

    it('validates the body', async () => {
        expect((await call(authed, 'POST', '/admin/publish', 'admin', { target: { type: 'connection' } })).status).toBe(400);
        expect((await call(authed, 'POST', '/admin/publish', 'admin', { target: { type: 'wat', id: 'x' } })).status).toBe(400);
    });

    it('is admin gated', async () => {
        const denied = await call(authed, 'POST', '/admin/publish', 'curator', { target: { type: 'connection', id: 'x' }, event: {} });
        expect(denied.status).toBe(403);
        expect((await call(authed, 'POST', '/admin/publish', undefined, { target: { type: 'connection', id: 'x' } })).status).toBe(401);
    });
});
