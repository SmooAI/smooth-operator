/**
 * Phase 7 provider path, SDK-side: the `corporate-proxy` demo registers an LLM
 * provider that proxies an OpenAI-compatible endpoint. Driven through
 * `createTestHost` against a REAL mock upstream (a node http server serving
 * scripted SSE + JSON + a token endpoint), so the whole chain is exercised:
 * `provider/complete` streaming → `provider/delta` chunks, non-streaming tool
 * calls, and the OAuth login/refresh handshake (with the login prompt answered
 * over `ui/request`).
 */
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { createTestHost, type ProviderStreamEvent, type TestHost, type UiRequestParams } from '../src/index.js';
import { createCorporateProxy } from '../examples/corporate-proxy.js';

// --- mock OpenAI-compatible upstream -------------------------------------

let server: Server;
let baseUrl: string;

/** Read + JSON-parse a request body. */
function readBody(req: IncomingMessage): Promise<Record<string, unknown>> {
    return new Promise((resolve) => {
        let raw = '';
        req.on('data', (c) => (raw += c));
        req.on('end', () => resolve(raw ? JSON.parse(raw) : {}));
    });
}

function sse(res: ServerResponse, chunks: string[]): void {
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    for (const c of chunks) res.write(`data: ${c}\n\n`);
    res.write('data: [DONE]\n\n');
    res.end();
}

beforeAll(async () => {
    server = createServer(async (req, res) => {
        if (req.url === '/oauth/token') {
            const body = await readBody(req);
            // Distinct tokens per grant so the test can tell login from refresh.
            const isRefresh = body.grant_type === 'refresh_token';
            res.writeHead(200, { 'content-type': 'application/json' });
            res.end(
                JSON.stringify({
                    access_token: isRefresh ? 'sk-refreshed' : 'sk-from-login',
                    refresh_token: 'rt-corp',
                    expires_in: 3600,
                }),
            );
            return;
        }
        if (req.url === '/chat/completions') {
            const body = await readBody(req);
            // Echo the presented bearer so a test can assert the key was used.
            const auth = req.headers.authorization ?? '';
            if (body.stream) {
                sse(res, [
                    JSON.stringify({ model: 'corp-gpt-4o-2026', choices: [{ delta: { content: 'Hel' } }] }),
                    JSON.stringify({ choices: [{ delta: { content: 'lo' } }] }),
                    JSON.stringify({ choices: [{ delta: { content: ` [${auth}]` } }], finish_reason: 'stop' }),
                    JSON.stringify({ usage: { prompt_tokens: 5, completion_tokens: 4, total_tokens: 9 } }),
                ]);
                return;
            }
            // Non-streaming: return a tool call so the mapping is exercised.
            res.writeHead(200, { 'content-type': 'application/json' });
            res.end(
                JSON.stringify({
                    model: 'corp-gpt-4o-2026',
                    choices: [
                        {
                            message: {
                                content: '',
                                tool_calls: [{ id: 'call_1', function: { name: 'get_weather', arguments: '{"city":"SF"}' } }],
                            },
                            finish_reason: 'tool_calls',
                        },
                    ],
                    usage: { prompt_tokens: 12, completion_tokens: 6, total_tokens: 18 },
                }),
            );
            return;
        }
        res.writeHead(404).end();
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    const addr = server.address();
    if (addr && typeof addr === 'object') baseUrl = `http://127.0.0.1:${addr.port}`;
});

afterAll(() => new Promise<void>((resolve) => server.close(() => resolve())));

let host: TestHost | undefined;
afterEach(() => host?.close());

function newHost(apiKey?: string, onUiRequest?: (p: UiRequestParams) => Record<string, unknown>): TestHost {
    const ext = createCorporateProxy({ baseUrl, ...(apiKey ? { apiKey } : {}) });
    return createTestHost(ext, onUiRequest ? { onUiRequest } : {});
}

describe('corporate-proxy provider registration', () => {
    it('surfaces the provider + models in the handshake', async () => {
        host = newHost('sk-static');
        const init = await host.initialize();
        const providers = init.registrations?.providers ?? [];
        expect(providers).toHaveLength(1);
        expect(providers[0]!.name).toBe('corporate-proxy');
        expect(providers[0]!.oauth).toBe(true);
        expect(providers[0]!.models?.map((m) => m.id)).toEqual(['corp-gpt-4o', 'corp-fast']);
    });
});

describe('proxied streaming', () => {
    it('streams provider/delta chunks then returns the assembled content', async () => {
        host = newHost('sk-static');
        await host.initialize();
        const deltas: string[] = [];
        const onDelta = (e: ProviderStreamEvent) => {
            if (e.type === 'Delta') deltas.push(e.content);
        };
        const result = await host.complete('corporate-proxy', 'corp-gpt-4o', [{ role: 'user', content: 'hi' }], { stream: true, onDelta });

        // The first two chunks streamed in order.
        expect(deltas.slice(0, 2)).toEqual(['Hel', 'lo']);
        // Final content is the concatenation, and it carried the bearer through.
        expect(result.content).toBe('Hello [Bearer sk-static]');
        expect(result.finish_reason).toBe('stop');
        expect(result.resolved_model).toBe('corp-gpt-4o-2026');
    });
});

describe('non-streaming completion', () => {
    it('maps an OpenAI tool-call response into the result', async () => {
        host = newHost('sk-static');
        await host.initialize();
        const result = await host.complete('corporate-proxy', 'corp-gpt-4o', [{ role: 'user', content: 'weather?' }]);
        expect(result.finish_reason).toBe('tool_calls');
        expect(result.tool_calls).toHaveLength(1);
        expect(result.tool_calls![0]!.name).toBe('get_weather');
        expect(result.tool_calls![0]!.arguments).toEqual({ city: 'SF' });
        expect(result.usage?.total_tokens).toBe(18);
    });
});

describe('OAuth handshake', () => {
    it('logs in via the ui prompt, then uses the obtained key for completions', async () => {
        // Answer the extension's `ui/request` input with a code.
        host = newHost(undefined, (p) => {
            if (p.kind === 'input') return { text: 'user-pasted-code' };
            return {};
        });
        await host.initialize({ ui_capabilities: ['notify', 'input'] });

        const creds = await host.oauthLogin('corporate-proxy');
        expect(creds.api_key).toBe('sk-from-login');
        expect(creds.refresh_token).toBe('rt-corp');
        expect(typeof creds.expires_at).toBe('number');

        // The login populated the live key — a subsequent streamed completion
        // presents it upstream.
        const result = await host.complete('corporate-proxy', 'corp-gpt-4o', [{ role: 'user', content: 'hi' }], { stream: true });
        expect(result.content).toContain('[Bearer sk-from-login]');
    });

    it('refreshes credentials with the presented refresh token', async () => {
        host = newHost('sk-static');
        await host.initialize();
        const creds = await host.oauthRefresh('corporate-proxy', 'rt-corp');
        expect(creds.api_key).toBe('sk-refreshed');
    });
});

describe('set_model', () => {
    it('an extension can switch to its own provider model at command tier', async () => {
        // A tiny command that calls session.setModel to select the provider model.
        const { defineExtension, defineCommand } = await import('../src/index.js');
        const ext = defineExtension((smooth) => {
            smooth.name = 'switcher';
            smooth.version = '0.0.1';
            smooth.registerCommand(
                defineCommand({
                    name: 'use-corp',
                    description: 'Switch to the corporate-proxy model.',
                    async execute(ctx) {
                        await ctx.session.setModel('corp-gpt-4o', { provider: 'corporate-proxy', thinking: 'medium' });
                        return 'switched';
                    },
                }),
            );
        });
        host = createTestHost(ext);
        await host.initialize();
        const out = await host.runCommand('use-corp');
        expect(out.content).toBe('switched');
        const setModel = host.sessionCalls.find((c) => c.method === 'session/set_model');
        expect(setModel?.params).toMatchObject({ model: 'corp-gpt-4o', provider: 'corporate-proxy', thinking: 'medium' });
    });
});
