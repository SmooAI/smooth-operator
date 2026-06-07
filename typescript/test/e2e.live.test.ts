/**
 * Live LLM WebSocket E2E — gated on `SMOOTH_AGENT_E2E=1` + `SMOOAI_GATEWAY_KEY`.
 *
 * This is the *real-transport* counterpart to `client.test.ts` (which drives the
 * client through an in-memory mock). Here we boot the actual Rust
 * `smooth-operator-agent-server` binary, connect the native
 * {@link SmoothAgentClient} over a real `ws://127.0.0.1:8810/ws` socket using
 * Node 22's global `WebSocket`, and drive real LLM turns through the live gateway.
 *
 * It asserts the same contract the Rust live E2E asserts, but exercised end-to-end
 * through the published TypeScript client + default {@link WebSocketTransport}:
 *   1. knowledge grounding — `send_message("What is SmooAI's return window?…")`
 *      streams ≥1 `stream_token`/`stream_chunk` and the terminal
 *      `eventual_response` text contains "17" (the seeded 17-day return fact).
 *   2. per-session memory — within the SAME session, "My name is Zog." then
 *      "What is my name?" → the reply contains "Zog".
 *
 * ## Gating (safe in CI without creds)
 *
 * This file is excluded from the default `vitest run` (see `vitest.config.ts`),
 * so `pnpm test` never touches it. It runs only via `pnpm test:e2e`. Even then it
 * skips cleanly unless BOTH are set:
 *   - `SMOOTH_AGENT_E2E=1`
 *   - `SMOOAI_GATEWAY_KEY=<key>` (read from this process's env; never printed)
 *
 * ## Run locally (does not print the key)
 *
 * ```sh
 * export SMOOAI_GATEWAY_KEY=$(python3 -c \
 *   "import json;print(json.load(open('$HOME/.local/share/opencode/auth.json'))['smooai']['key'])")
 * export SMOOTH_AGENT_E2E=1
 * pnpm test:e2e
 * ```
 */
import { spawn, type ChildProcess } from 'node:child_process';
import { connect as tcpConnect } from 'node:net';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { SmoothAgentClient } from '../src/client.js';
import type { ServerEvent } from '../src/types.js';

// ───────────────────────────── Gating ──────────────────────────────────────

const GATEWAY_KEY = process.env.SMOOAI_GATEWAY_KEY?.trim();
const E2E_ENABLED = process.env.SMOOTH_AGENT_E2E === '1' && !!GATEWAY_KEY;

if (!E2E_ENABLED) {
    const why =
        process.env.SMOOTH_AGENT_E2E !== '1'
            ? 'SMOOTH_AGENT_E2E != "1"'
            : 'SMOOAI_GATEWAY_KEY unset/empty';
    // eslint-disable-next-line no-console
    console.log(`[skip] live WS E2E: ${why} — skipping live-gateway test (this is expected in CI).`);
}

// ──────────────────────────── Configuration ─────────────────────────────────

/** Unique port for this harness (see CLAUDE.md task contract). */
const PORT = 8810;
const WS_URL = `ws://127.0.0.1:${PORT}/ws`;
/** Binary built by `cargo build -p smooai-smooth-operator-agent-server`. */
const SERVER_BIN = `${process.env.HOME}/.cargo/shared-target/debug/smooth-operator-agent-server`;
/** Overall budget per real LLM turn — the gateway + tool loop can be slow. */
const TURN_TIMEOUT_MS = 120_000;

// ───────────────────────────── Helpers ─────────────────────────────────────

/** Resolve once a TCP connection to `127.0.0.1:port` succeeds, or reject on timeout. */
function waitForPort(port: number, timeoutMs: number): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    return new Promise((resolve, reject) => {
        const attempt = (): void => {
            const socket = tcpConnect(port, '127.0.0.1');
            socket.once('connect', () => {
                socket.destroy();
                resolve();
            });
            socket.once('error', () => {
                socket.destroy();
                if (Date.now() > deadline) {
                    reject(new Error(`Server did not open port ${port} within ${timeoutMs}ms`));
                } else {
                    setTimeout(attempt, 100);
                }
            });
        };
        attempt();
    });
}

/**
 * Pull the final assistant text from an `eventual_response` event. The runner
 * puts the reply in `data.data.response.responseParts[]` (joined), falling back
 * to a plain-string response.
 */
function finalText(eventual: ServerEvent): string {
    const resp = (eventual as { data?: { data?: { response?: unknown } } }).data?.data?.response;
    if (resp && typeof resp === 'object' && Array.isArray((resp as { responseParts?: unknown }).responseParts)) {
        return ((resp as { responseParts: unknown[] }).responseParts.filter((p): p is string => typeof p === 'string')).join(' ');
    }
    if (typeof resp === 'string') return resp;
    return '';
}

/**
 * Drive one streaming turn end-to-end through the client. Collects every event
 * up to and including the terminal `eventual_response`, returning the events,
 * the final event, and the joined reply text.
 */
async function runTurn(
    client: SmoothAgentClient,
    sessionId: string,
    message: string,
): Promise<{ events: ServerEvent[]; eventual: ServerEvent; reply: string }> {
    const turn = client.sendMessage({ sessionId, message });
    const events: ServerEvent[] = [];
    for await (const ev of turn) events.push(ev);
    const eventual = await turn; // resolves with the terminal EventualResponse
    return { events, eventual: eventual as ServerEvent, reply: finalText(eventual as ServerEvent) };
}

// ─────────────────────────────── Suite ─────────────────────────────────────

describe.skipIf(!E2E_ENABLED)('live WS E2E — real Rust server + real LLM', () => {
    let server: ChildProcess;
    let client: SmoothAgentClient;

    beforeAll(async () => {
        // Boot the real binary, passing the gateway key through to ITS env only
        // (never logged here). Unique port + seeded KB + cheap model.
        server = spawn(SERVER_BIN, [], {
            env: {
                ...process.env,
                SMOOTH_AGENT_PORT: String(PORT),
                SMOOTH_AGENT_SEED_KB: '1',
                SMOOTH_AGENT_MODEL: 'claude-haiku-4-5',
                SMOOAI_GATEWAY_KEY: GATEWAY_KEY,
            },
            stdio: ['ignore', 'pipe', 'pipe'],
        });
        server.stdout?.on('data', (d) => process.stdout.write(`[server] ${d}`));
        server.stderr?.on('data', (d) => process.stderr.write(`[server] ${d}`));
        server.once('exit', (code, signal) => {
            if (code != null && code !== 0) {
                // Surface an early crash (e.g. bad binary) rather than hanging on the port wait.
                console.error(`[server] exited early code=${code} signal=${signal}`);
            }
        });

        await waitForPort(PORT, 20_000);

        // Default WebSocketTransport uses Node 22's global WebSocket — no `ws` devDep needed.
        client = new SmoothAgentClient({ url: WS_URL, requestTimeout: TURN_TIMEOUT_MS });
        await client.connect();
    }, 30_000);

    afterAll(() => {
        try {
            client?.disconnect('e2e done');
        } catch {
            /* best-effort */
        }
        if (server && !server.killed) server.kill('SIGTERM');
    });

    it(
        'creates a session, streams a knowledge-grounded answer (17), and remembers (Zog)',
        async () => {
            // 1. Create a session over the real socket.
            const session = await client.createConversationSession({ agentId: 'e2e' });
            expect(session.sessionId, 'session creation must return a sessionId').toBeTruthy();
            console.log(`[live-ws] session: ${session.sessionId}`);

            // 2. Knowledge-grounded turn — expect streaming + the seeded "17"-day fact.
            const turn1 = await runTurn(
                client,
                session.sessionId,
                "What is SmooAI's return window? Search the knowledge base.",
            );
            const streamTokens = turn1.events.filter((e) => e.type === 'stream_token').length;
            const streamChunks = turn1.events.filter((e) => e.type === 'stream_chunk').length;
            const tokenSample = turn1.events
                .filter((e): e is Extract<ServerEvent, { type: 'stream_token' }> => e.type === 'stream_token')
                .map((e) => e.token ?? '')
                .join('')
                .slice(0, 120);
            console.log(`[live-ws] turn 1 streamed: ${streamTokens} stream_token, ${streamChunks} stream_chunk events`);
            console.log(`[live-ws] turn 1 token sample: ${JSON.stringify(tokenSample)}`);
            console.log(`[live-ws] turn 1 final reply: ${JSON.stringify(turn1.reply)}`);

            expect(
                streamTokens + streamChunks,
                'expected at least one streamed stream_token or stream_chunk event',
            ).toBeGreaterThanOrEqual(1);
            expect(turn1.reply, 'grounded answer should contain the retrieved 17-day fact').toContain('17');

            // 3a. Tell the agent a name to remember (same session).
            const turn2 = await runTurn(client, session.sessionId, 'My name is Zog. Just acknowledge briefly.');
            console.log(`[live-ws] turn 2 reply: ${JSON.stringify(turn2.reply)}`);

            // 3b. Ask it back — assert per-session memory recalled "Zog".
            const turn3 = await runTurn(client, session.sessionId, 'What is my name? Reply with just the name.');
            console.log(`[live-ws] turn 3 reply (memory check): ${JSON.stringify(turn3.reply)}`);
            expect(turn3.reply.toUpperCase(), 'per-session memory should recall "Zog"').toContain('ZOG');
        },
        TURN_TIMEOUT_MS * 3 + 30_000,
    );
});
