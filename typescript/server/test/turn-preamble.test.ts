/**
 * The optional fast-model preamble (`SMOOTH_AGENT_PREAMBLE_MODEL`, pearl th-9a5794).
 *
 * This feature is mostly defined by what must NOT happen — no extra call when it is
 * off, no event once the real answer started, no error surfaced when it fails, and
 * never a byte of it in the persisted reply — so the tests below lean on the negatives.
 *
 * Every ordering assertion is DETERMINISTIC: the fake chat client gates the agent's
 * stream on a promise the test resolves, and `flush()` drains the microtask queue at a
 * `setImmediate` boundary (an event-loop guarantee, not a sleep).
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import type { ChatClientLike } from '@smooai/smooth-operator-core';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { Frame } from '../src/protocol.js';
import { serve, type RunningServer } from '../src/server.js';
import { InMemorySessionStore } from '../src/sessionStore.js';
import { PREAMBLE_MAX_TOKENS, PREAMBLE_SYSTEM_PROMPT, runPreamble, TurnRunner } from '../src/turnRunner.js';
import { TestClient } from './wsClient.js';

const PREAMBLE_MODEL = 'fast-preamble-model';
const PREAMBLE_TEXT = 'Let me pull up your recent conversations.';
const ANSWER = 'Your return window is 17 days.';

/** Resolve at the next `setImmediate` — i.e. after every pending microtask has run. */
const flush = (): Promise<void> => new Promise<void>((resolve) => setImmediate(resolve));

interface Deferred<T> {
    promise: Promise<T>;
    resolve: (value: T) => void;
    reject: (err: Error) => void;
}

function deferred<T>(): Deferred<T> {
    let resolve!: (value: T) => void;
    let reject!: (err: Error) => void;
    const promise = new Promise<T>((res, rej) => {
        resolve = res;
        reject = rej;
    });
    return { promise, resolve, reject };
}

/**
 * A chat client that routes the two seams apart: `createStream` (the agent loop) goes
 * to a scripted {@link MockLlmProvider}, while `create` — which in these turns is ONLY
 * the preamble, since no workflow judge is configured — is answered by the test.
 *
 * `agentGate` lets a test hold the answer back until the preamble has landed (or
 * release it first, to exercise the race guard).
 */
class RoutingChatClient implements ChatClientLike {
    readonly preambleCalls: Record<string, unknown>[] = [];

    constructor(
        private readonly agent: MockLlmProvider,
        private readonly preamble: () => Promise<string>,
        private readonly agentGate?: Promise<unknown>,
    ) {}

    readonly chat = {
        completions: {
            create: async (body: Record<string, unknown>) => {
                this.preambleCalls.push(body);
                const content = await this.preamble();
                return { choices: [{ message: { content } }], usage: null };
            },
            createStream: (body: Record<string, unknown>) => {
                const agent = this.agent;
                const gate = this.agentGate;
                return (async function* () {
                    if (gate) await gate;
                    yield* agent.chat.completions.createStream(body);
                })();
            },
        },
    };
}

/** Drive one turn through a fresh conversation, collecting every emitted frame. */
async function runTurn(chatClient: ChatClientLike, store = new InMemorySessionStore()): Promise<{ frames: Frame[]; reply: string; store: InMemorySessionStore }> {
    const session = await store.createSession('agent-1');
    const runner = new TurnRunner({ chatClient, store });
    const frames: Frame[] = [];
    const result = await runner.run(session.conversationId, 'req-1', 'How long can I return?', (frame) => frames.push(frame));
    return { frames, reply: result.reply, store };
}

const preambleFrames = (frames: Frame[]): Frame[] => frames.filter((f) => f.type === 'stream_preamble');

describe('fast-model preamble', () => {
    const original = process.env.SMOOTH_AGENT_PREAMBLE_MODEL;

    beforeEach(() => {
        delete process.env.SMOOTH_AGENT_PREAMBLE_MODEL;
    });

    afterEach(() => {
        if (original === undefined) delete process.env.SMOOTH_AGENT_PREAMBLE_MODEL;
        else process.env.SMOOTH_AGENT_PREAMBLE_MODEL = original;
    });

    describe('off by default', () => {
        it('emits nothing and never calls the preamble model when the env var is unset', async () => {
            const client = new RoutingChatClient(new MockLlmProvider().pushText(ANSWER), async () => {
                throw new Error('the preamble model must not be called when the feature is off');
            });

            const { frames, reply } = await runTurn(client);
            await flush();

            expect(client.preambleCalls).toHaveLength(0);
            expect(preambleFrames(frames)).toHaveLength(0);
            expect(reply).toBe(ANSWER);
        });

        it.each([
            ['empty', ''],
            ['whitespace', '   '],
        ])('treats an %s env value as off', async (_label, value) => {
            process.env.SMOOTH_AGENT_PREAMBLE_MODEL = value;
            const client = new RoutingChatClient(new MockLlmProvider().pushText(ANSWER), async () => {
                throw new Error('the preamble model must not be called for a blank env value');
            });

            const { frames } = await runTurn(client);
            await flush();

            expect(client.preambleCalls).toHaveLength(0);
            expect(preambleFrames(frames)).toHaveLength(0);
        });
    });

    describe('on', () => {
        it('emits the documented wire shape and calls the fast model with the verbatim prompt', async () => {
            process.env.SMOOTH_AGENT_PREAMBLE_MODEL = PREAMBLE_MODEL;
            // Hold the answer until the preamble has been emitted, so the guard can't
            // legitimately drop it — this test is about the shape, not the race.
            const emitted = deferred<void>();
            const client = new RoutingChatClient(new MockLlmProvider().pushText(ANSWER), async () => PREAMBLE_TEXT, emitted.promise);

            const store = new InMemorySessionStore();
            const session = await store.createSession('agent-1');
            const frames: Frame[] = [];
            const before = Date.now();
            await new TurnRunner({ chatClient: client, store }).run(session.conversationId, 'req-1', 'How long can I return?', (frame) => {
                frames.push(frame);
                if (frame.type === 'stream_preamble') emitted.resolve();
            });

            const preambles = preambleFrames(frames);
            expect(preambles).toHaveLength(1);
            const frame = preambles[0];
            expect(frame).toMatchObject({
                type: 'stream_preamble',
                requestId: 'req-1',
                token: PREAMBLE_TEXT,
                data: { requestId: 'req-1', token: PREAMBLE_TEXT },
            });
            // `timestamp` is epoch millis, and the frame carries nothing else.
            expect(typeof frame.timestamp).toBe('number');
            expect(frame.timestamp as number).toBeGreaterThanOrEqual(before);
            expect(Object.keys(frame).sort()).toEqual(['data', 'requestId', 'timestamp', 'token', 'type']);

            // Same client (gateway + key); only the model id and the token cap differ, and
            // the user's message is the ONLY user-role content (no tool results, no history).
            expect(client.preambleCalls).toHaveLength(1);
            const body = client.preambleCalls[0];
            expect(body.model).toBe(PREAMBLE_MODEL);
            expect(body.max_tokens).toBe(PREAMBLE_MAX_TOKENS);
            expect(PREAMBLE_MAX_TOKENS).toBe(64);
            expect(body.messages).toEqual([
                { role: 'system', content: PREAMBLE_SYSTEM_PROMPT },
                { role: 'user', content: 'How long can I return?' },
            ]);
        });

        it('trims the model output and drops an all-whitespace preamble', async () => {
            process.env.SMOOTH_AGENT_PREAMBLE_MODEL = PREAMBLE_MODEL;
            const emitted = deferred<void>();
            const client = new RoutingChatClient(new MockLlmProvider().pushText(ANSWER), async () => `  ${PREAMBLE_TEXT}\n`, emitted.promise);
            const store = new InMemorySessionStore();
            const session = await store.createSession('agent-1');
            const frames: Frame[] = [];
            await new TurnRunner({ chatClient: client, store }).run(session.conversationId, 'req-1', 'hi', (frame) => {
                frames.push(frame);
                if (frame.type === 'stream_preamble') emitted.resolve();
            });
            expect(preambleFrames(frames)[0].token).toBe(PREAMBLE_TEXT);

            // Blank output → nothing on the wire.
            const blankClient = new RoutingChatClient(new MockLlmProvider().pushText(ANSWER), async () => '   ');
            const { frames: blankFrames } = await runTurn(blankClient);
            await flush();
            expect(preambleFrames(blankFrames)).toHaveLength(0);
        });
    });

    describe('the race guard', () => {
        it('emits nothing once the answer has started (unit, deterministic)', async () => {
            const gate = deferred<string>();
            const client = new RoutingChatClient(new MockLlmProvider(), () => gate.promise);
            const frames: Frame[] = [];
            const answerStarted = { started: false };

            const pending = runPreamble(client, PREAMBLE_MODEL, 'req-1', 'hi', (f) => frames.push(f), answerStarted);
            // The real answer beats the fast model to the punch…
            answerStarted.started = true;
            // …and only THEN does the preamble come back.
            gate.resolve(PREAMBLE_TEXT);
            await pending;

            expect(frames).toHaveLength(0);
        });

        it('still emits when the answer has not started', async () => {
            const gate = deferred<string>();
            const client = new RoutingChatClient(new MockLlmProvider(), () => gate.promise);
            const frames: Frame[] = [];
            const pending = runPreamble(client, PREAMBLE_MODEL, 'req-1', 'hi', (f) => frames.push(f), { started: false });
            gate.resolve(PREAMBLE_TEXT);
            await pending;
            expect(preambleFrames(frames)).toHaveLength(1);
        });

        it('drops a slow preamble that resolves after a whole turn finished streaming', async () => {
            process.env.SMOOTH_AGENT_PREAMBLE_MODEL = PREAMBLE_MODEL;
            // Ungated agent stream + a preamble the test resolves only after `run` returned.
            const gate = deferred<string>();
            const client = new RoutingChatClient(new MockLlmProvider().pushText(ANSWER), () => gate.promise);

            const { frames, reply } = await runTurn(client);
            expect(reply).toBe(ANSWER);
            expect(frames.some((f) => f.type === 'stream_token')).toBe(true);

            gate.resolve(PREAMBLE_TEXT);
            await flush();

            expect(client.preambleCalls).toHaveLength(1); // it really did run…
            expect(preambleFrames(frames)).toHaveLength(0); // …and was suppressed.
        });
    });

    describe('best-effort failure handling', () => {
        it('completes the turn with no error event and no unhandled rejection when the preamble throws', async () => {
            process.env.SMOOTH_AGENT_PREAMBLE_MODEL = PREAMBLE_MODEL;
            const unhandled: unknown[] = [];
            const onUnhandled = (err: unknown): void => {
                unhandled.push(err);
            };
            process.on('unhandledRejection', onUnhandled);
            try {
                const gate = deferred<string>();
                const client = new RoutingChatClient(new MockLlmProvider().pushText(ANSWER), () => gate.promise);

                const { frames, reply } = await runTurn(client);
                gate.reject(new Error('gateway exploded'));
                await flush();
                await flush();

                expect(reply).toBe(ANSWER);
                expect(frames.some((f) => f.type === 'error')).toBe(false);
                expect(preambleFrames(frames)).toHaveLength(0);
                expect(unhandled).toHaveLength(0);
            } finally {
                process.off('unhandledRejection', onUnhandled);
            }
        });
    });

    describe('ephemerality', () => {
        it('never persists the preamble nor folds it into the reply', async () => {
            process.env.SMOOTH_AGENT_PREAMBLE_MODEL = PREAMBLE_MODEL;
            const emitted = deferred<void>();
            const client = new RoutingChatClient(new MockLlmProvider().pushText(ANSWER), async () => PREAMBLE_TEXT, emitted.promise);

            const store = new InMemorySessionStore();
            const session = await store.createSession('agent-1');
            const frames: Frame[] = [];
            const result = await new TurnRunner({ chatClient: client, store }).run(session.conversationId, 'req-1', 'How long can I return?', (frame) => {
                frames.push(frame);
                if (frame.type === 'stream_preamble') emitted.resolve();
            });

            expect(preambleFrames(frames)).toHaveLength(1); // it WAS shown…
            expect(result.reply).toBe(ANSWER); // …but never became the answer.
            const persisted = await store.listMessages(session.conversationId, 50);
            expect(persisted.some((m) => m.text.includes(PREAMBLE_TEXT))).toBe(false);
            expect(persisted.some((m) => m.direction === 'outbound' && m.text === ANSWER)).toBe(true);
        });

        it('keeps the preamble out of eventual_response over a real WebSocket', async () => {
            process.env.SMOOTH_AGENT_PREAMBLE_MODEL = PREAMBLE_MODEL;
            let releaseAnswer!: () => void;
            const answerGate = new Promise<void>((resolve) => {
                releaseAnswer = resolve;
            });
            const client = new RoutingChatClient(new MockLlmProvider().pushText(ANSWER), async () => {
                // Release the answer only once the preamble is on its way back, so the
                // ordering (preamble first, then the answer) is fixed, not timing-based.
                setImmediate(releaseAnswer);
                return PREAMBLE_TEXT;
            }, answerGate);

            let server: RunningServer | undefined;
            try {
                server = await serve({ chatClient: client });
                const ws = await TestClient.connect(server.url);
                ws.sendAction({ action: 'create_conversation_session', requestId: 'cs' });
                const sessionId = ((await ws.receive()).data as Record<string, unknown>).sessionId as string;

                ws.sendAction({ action: 'send_message', requestId: 'sm', sessionId, message: 'How long can I return?' });
                const { terminal, seen } = await ws.receiveUntil('eventual_response');

                expect(preambleFrames(seen)).toHaveLength(1);
                const inner = (terminal.data as Record<string, unknown>).data as Record<string, unknown>;
                const parts = (inner.response as Record<string, unknown>).responseParts as string[];
                expect(parts.join(' ')).toContain('17 days');
                expect(parts.join(' ')).not.toContain(PREAMBLE_TEXT);

                await ws.close();
            } finally {
                await server?.close();
            }
        });
    });
});
