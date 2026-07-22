/**
 * User-initiated turn cancellation — the `cancel` action ("Stop button").
 *
 * The TS parity port of the Rust reference's `rust/smooth-operator-server/tests/turn_cancel.rs`.
 * It proves, over a real WebSocket:
 *
 *   1. **Cancel mid-turn stops it.** A `cancel` frame while a turn is parked in a tool
 *      aborts the turn — a terminal `cancelled` event (status 499) is emitted, no
 *      `eventual_response` follows, the model is never called again, and the partial
 *      assistant reply is DISCARDED while the user's message stays persisted.
 *   2. **Cancel with no active turn is a silent no-op** (no event; connection stays live).
 *   3. **A normal turn still completes** with an `eventual_response` (the cancellation
 *      wiring doesn't disturb the happy path).
 *   4. **Disconnect mid-turn also aborts the turn** (no client remains to receive it).
 *
 * Plus the single-active-turn rule the reference enforces alongside cancel: a second
 * `send_message` while one is in flight is rejected with `TURN_IN_PROGRESS`.
 *
 * Runs fully offline: a `MockLlmProvider` scripts the turn and a host tool parks it on a
 * gate the test releases, giving a stable in-flight window to cancel in.
 *
 * Where this differs from Rust: JS cannot DROP an in-flight `await` the way tokio's
 * `abort()` drops a future, so the reference's drop-guard assertion has no TS analogue.
 * Cancellation is cooperative — what is asserted instead is the observable protocol
 * contract (terminal `cancelled`, no `eventual_response`, no further model call, no
 * persisted reply), which is the part every language server must match.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import type { Tool } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { serve, type RunningServer } from '../src/server.js';
import { InMemorySessionStore } from '../src/sessionStore.js';
import { TestClient } from './wsClient.js';

const SLOW_TOOL = 'slow_probe';

/**
 * A tool that PARKS the turn: it records that it started, then blocks on a gate the test
 * controls. The TS stand-in for the Rust test's hour-long `tokio::time::sleep` — with an
 * explicit release so no timer outlives the test.
 */
class SlowTool {
    started = false;
    finished = false;
    private release!: () => void;
    private readonly gate = new Promise<void>((resolve) => {
        this.release = resolve;
    });

    /** Let the parked tool return. */
    finish(): void {
        this.release();
    }

    tool(): Tool {
        return {
            name: SLOW_TOOL,
            description: 'parks the turn for cancellation tests',
            parameters: { type: 'object', properties: {} },
            execute: async (): Promise<string> => {
                this.started = true;
                await this.gate;
                this.finished = true;
                return 'done';
            },
        };
    }
}

/** A mock that calls the slow tool (so the turn parks in it), then would wrap up. */
function slowToolMock(): MockLlmProvider {
    const mock = new MockLlmProvider();
    mock.pushToolCall('call_1', SLOW_TOOL, '{}');
    mock.pushText('All done here.');
    return mock;
}

/** A mock that just answers (no tool), so a turn completes normally. */
function answerMock(text: string): MockLlmProvider {
    const mock = new MockLlmProvider();
    mock.pushText(text);
    return mock;
}

/** Drive `create_conversation_session` and return the session + conversation ids. */
async function createSession(client: TestClient): Promise<{ sessionId: string; conversationId: string }> {
    client.sendAction({ action: 'create_conversation_session', requestId: 'cs-1', agentId: '11111111-1111-1111-1111-111111111111' });
    const created = await client.receive();
    expect(created.type).toBe('immediate_response');
    const data = created.data as { sessionId: string; conversationId: string };
    return { sessionId: data.sessionId, conversationId: data.conversationId };
}

/** Try to receive one event within `ms`; `undefined` when none arrives (the no-event assertion). */
async function recvWithin(client: TestClient, ms: number): Promise<Record<string, unknown> | undefined> {
    try {
        return await client.receive(ms);
    } catch {
        return undefined;
    }
}

/** Poll `predicate` until it holds or the deadline elapses. */
async function waitFor(predicate: () => boolean, timeoutMs = 5000): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
        if (predicate()) return;
        await new Promise((r) => setTimeout(r, 10));
    }
    throw new Error('condition not met within timeout');
}

describe('turn cancellation — the cancel action', () => {
    let server: RunningServer | undefined;
    let slow: SlowTool | undefined;

    afterEach(async () => {
        // Release any parked tool first so teardown never waits on the gate.
        slow?.finish();
        slow = undefined;
        await server?.close();
        server = undefined;
    });

    it('cancel mid-turn aborts the turn and emits a terminal cancelled event', async () => {
        slow = new SlowTool();
        const mock = slowToolMock();
        const store = new InMemorySessionStore();
        server = await serve({ chatClient: mock, store, tools: [slow.tool()] });

        const client = await TestClient.connect(server.url);
        try {
            const { sessionId, conversationId } = await createSession(client);
            client.sendAction({ action: 'send_message', requestId: 'turn-1', sessionId, message: 'please do the slow thing' });

            const ack = await client.receive();
            expect(ack.status).toBe(202);

            // Wait until the turn is genuinely in flight (parked inside the tool).
            await waitFor(() => slow!.started);
            expect(slow.finished).toBe(false);

            // Cancel it, reusing the turn's requestId (the correlation convention).
            client.sendAction({ action: 'cancel', requestId: 'turn-1' });

            const { terminal } = await client.receiveUntil('cancelled');
            expect(terminal.requestId).toBe('turn-1');
            expect(terminal.status).toBe(499);
            expect(terminal.data).toEqual({ requestId: 'turn-1', status: 499 });

            // Let the parked tool return: the turn observes the cancel and stops there.
            slow.finish();

            // No further terminal event (no eventual_response) follows the cancellation…
            const after = await recvWithin(client, 500);
            expect(after).toBeUndefined();
            // …and the turn never continued: the model was never called a second time.
            expect(mock.callCount).toBe(1);

            // Partial output discarded: the user's message stays persisted, the assistant
            // reply never is.
            const messages = await store.listMessages(conversationId, 50);
            expect(messages.map((m) => m.direction)).toEqual(['inbound']);

            // The connection is still alive and usable.
            client.sendAction({ action: 'ping', requestId: 'p1' });
            const pong = await client.receive();
            expect(pong.type).toBe('pong');
            expect(pong.requestId).toBe('p1');
        } finally {
            await client.close();
        }
    });

    it('cancel with no active turn is a silent no-op', async () => {
        server = await serve({ chatClient: answerMock('hi') });
        const client = await TestClient.connect(server.url);
        try {
            await createSession(client);

            client.sendAction({ action: 'cancel', requestId: 'nope' });

            // The next event is the pong — the cancel produced no event of its own.
            client.sendAction({ action: 'ping', requestId: 'p1' });
            const event = await client.receive();
            expect(event.type).toBe('pong');
            expect(event.requestId).toBe('p1');
        } finally {
            await client.close();
        }
    });

    it('a normal turn still completes with an eventual_response', async () => {
        server = await serve({ chatClient: answerMock('All done here.') });
        const client = await TestClient.connect(server.url);
        try {
            const { sessionId } = await createSession(client);
            client.sendAction({ action: 'send_message', requestId: 'turn-ok', sessionId, message: 'hello' });

            const { terminal, seen } = await client.receiveUntil('eventual_response');
            expect(terminal.requestId).toBe('turn-ok');
            expect(terminal.status).toBe(200);
            expect(seen.some((e) => e.type === 'cancelled')).toBe(false);
        } finally {
            await client.close();
        }
    });

    it('disconnect mid-turn aborts the in-flight turn', async () => {
        slow = new SlowTool();
        const mock = slowToolMock();
        const store = new InMemorySessionStore();
        server = await serve({ chatClient: mock, store, tools: [slow.tool()] });

        const client = await TestClient.connect(server.url);
        const { sessionId, conversationId } = await createSession(client);
        client.sendAction({ action: 'send_message', requestId: 'turn-x', sessionId, message: 'please do the slow thing' });
        expect((await client.receive()).status).toBe(202);
        await waitFor(() => slow!.started);

        // Client hangs up mid-turn. `close()` resolves on the CLIENT's socket; give the
        // server a beat to observe the close and abort the turn before releasing the tool.
        await client.close();
        await new Promise((r) => setTimeout(r, 200));

        // Release the parked tool; the server must abort rather than finish the turn.
        slow.finish();
        await new Promise((r) => setTimeout(r, 300));

        expect(mock.callCount).toBe(1);
        const messages = await store.listMessages(conversationId, 50);
        expect(messages.map((m) => m.direction)).toEqual(['inbound']);
    });

    it('a second send_message while a turn is in flight is rejected with TURN_IN_PROGRESS', async () => {
        slow = new SlowTool();
        const store = new InMemorySessionStore();
        server = await serve({ chatClient: slowToolMock(), store, tools: [slow.tool()] });

        const client = await TestClient.connect(server.url);
        try {
            const { sessionId } = await createSession(client);
            client.sendAction({ action: 'send_message', requestId: 'turn-1', sessionId, message: 'first' });
            expect((await client.receive()).status).toBe(202);
            await waitFor(() => slow!.started);

            client.sendAction({ action: 'send_message', requestId: 'turn-2', sessionId, message: 'second' });
            // Skip the first turn's in-flight stream frames (its toolCall chunk).
            const { terminal: err } = await client.receiveUntil('error');
            expect(err.requestId).toBe('turn-2');
            expect((err.error as { code: string }).code).toBe('TURN_IN_PROGRESS');

            // Cancelling the first frees the connection for a new turn.
            client.sendAction({ action: 'cancel', requestId: 'turn-1' });
            const { terminal } = await client.receiveUntil('cancelled');
            expect(terminal.requestId).toBe('turn-1');
            slow.finish();
        } finally {
            await client.close();
        }
    });
});
