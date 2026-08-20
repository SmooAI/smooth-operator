/**
 * A CANCELLED turn's LATE teardown must not unpark the turn that replaced it.
 *
 * Cancellation in the TypeScript server is COOPERATIVE — `cancelActiveTurn()` frees the
 * connection's turn slot synchronously, but the cancelled turn itself keeps running until
 * its next stream event, which can be a long tool call away. Meanwhile the client is free
 * to start a new turn on the same session immediately, and that turn may park on a
 * write-confirmation of its own. The teardown registries (`ConfirmationRegistry`,
 * `InteractionParkRegistry`) key on `sessionId`, NOT on turn identity, so the cancelled
 * turn's eventual `finally` used to clear the SUCCESSOR's registration: the client's
 * `confirm_tool_action` came back `NO_PENDING_CONFIRMATION` and — because nothing else
 * ever settles a confirmation short of a disconnect — the successor turn hung forever.
 *
 * The Rust reference cannot reach this: `handle.abort()` drops the turn future, so the
 * `(cfg.clear)` statements after the executor await never run on the aborted path.
 *
 * Two tests, both failing before the fix and differing from `cancel-unpark.test.ts` on
 * the two points that matter: turn 1 is parked in a SLOW TOOL (not at the confirmation),
 * and turn 2 REGISTERS a confirmation of its own rather than being a plain text answer.
 *
 *   1. `TurnRunner` level — the barrier is `await expect(turnA).rejects…`, so turn A's
 *      `finally` has provably run before the assertions. No timing at all.
 *   2. Over a real socket — the whole client-visible sequence from the report.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import type { Tool } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { ConfirmationRegistry } from '../src/confirmation.js';
import { serve, type RunningServer } from '../src/server.js';
import { InMemorySessionStore } from '../src/sessionStore.js';
import { TurnCancelledError, TurnRunner } from '../src/turnRunner.js';
import { TestClient } from './wsClient.js';

const SLOW_TOOL = 'fetch_report';
const GATED_TOOL = 'delete_record';
const SESSION_ID = '22222222-2222-2222-2222-222222222222';

/** A pair of promises: one that settles when the tool is entered, one the test releases. */
interface SlowToolHandle {
    tool: Tool;
    /** Resolves once the tool body has started — turn 1 is provably inside it. */
    entered: Promise<void>;
    /** Let the tool return, resuming the cancelled turn into its teardown. */
    release(): void;
}

/**
 * A tool that blocks until the test releases it. This is the whole point of the
 * reproduction: the cancelled turn must be somewhere it cannot observe the abort, so its
 * teardown lands LATE — after the next turn has parked.
 */
function slowTool(): SlowToolHandle {
    let markEntered!: () => void;
    let release!: () => void;
    const entered = new Promise<void>((resolve) => {
        markEntered = resolve;
    });
    const gate = new Promise<void>((resolve) => {
        release = resolve;
    });
    return {
        entered,
        release,
        tool: {
            name: SLOW_TOOL,
            description: 'Fetch a long-running report.',
            parameters: { type: 'object', properties: {} },
            execute: async (): Promise<string> => {
                markEntered();
                await gate;
                return 'Report ready.';
            },
        },
    };
}

/** The confirm-gated write tool turn 2 parks on. */
function gatedTool(): Tool {
    return {
        name: GATED_TOOL,
        description: 'Delete a record by id (a state-mutating write).',
        parameters: { type: 'object', properties: { id: { type: 'string' } }, required: ['id'] },
        execute: async (): Promise<string> => 'Record 42 deleted.',
    };
}

describe('late teardown of a cancelled turn — turn scoping', () => {
    it("a cancelled turn's finally leaves the NEXT turn's confirmation registered", async () => {
        // ONE registry, shared by both turns exactly as a connection shares it.
        const registry = new ConfirmationRegistry();
        const slow = slowTool();
        const mock = new MockLlmProvider();
        mock.pushToolCall('call-a', SLOW_TOOL, '{}');

        const runner = new TurnRunner({
            chatClient: mock,
            store: new InMemorySessionStore(),
            tools: [slow.tool, gatedTool()],
            confirmTools: [GATED_TOOL],
            confirmations: registry,
            sessionId: SESSION_ID,
        });

        const cancel = new AbortController();
        const turnA = runner.run('conv-1', 'turn-1', 'fetch the report', () => {}, undefined, cancel.signal);

        // Turn A is provably inside the slow tool — it cannot observe the abort from here.
        await slow.entered;

        // Cancel. This is what `cancelActiveTurn()` does: fire the abort, then discard the
        // session's pending confirmation (a no-op — turn A is in a tool, not parked).
        cancel.abort();
        expect(registry.resolve(SESSION_ID, false)).toBe(false);

        // Turn B starts on the same session and parks on a gated tool.
        const verdictB = registry.register(SESSION_ID);

        // Now turn A's tool returns and it finally observes the cancel. Awaiting the
        // rejection is the barrier: turn A's `finally` has run by the time this resolves.
        slow.release();
        await expect(turnA).rejects.toBeInstanceOf(TurnCancelledError);

        // Turn B's registration must have survived: its `confirm_tool_action` resolves.
        expect(registry.resolve(SESSION_ID, true)).toBe(true);
        await expect(verdictB).resolves.toBe(true);
    });
});

describe('late teardown of a cancelled turn — over a real socket', () => {
    let server: RunningServer | undefined;

    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('a turn parked on a confirmation still resumes after the cancelled turn tears down', async () => {
        const slow = slowTool();
        // Turn 1 calls the slow tool; turn 2 calls the gated tool, then answers.
        // Turn 1 never makes a second model call — the engine yields `tool_result` before
        // requesting again, and the runner throws there — so the script stays in order.
        const mock = new MockLlmProvider();
        mock.pushToolCall('call-a', SLOW_TOOL, '{}');
        mock.pushToolCall('call-b', GATED_TOOL, '{"id": "42"}');
        mock.pushText('Record 42 is gone.');

        server = await serve({
            chatClient: mock,
            store: new InMemorySessionStore(),
            tools: [slow.tool, gatedTool()],
            confirmTools: [GATED_TOOL],
        });
        const client = await TestClient.connect(server.url);
        const all: Record<string, unknown>[] = [];
        const drain = async (type: string): Promise<Record<string, unknown>> => {
            const { terminal, seen } = await client.receiveUntil(type);
            all.push(...seen);
            return terminal;
        };
        try {
            client.sendAction({ action: 'create_conversation_session', requestId: 'cs', agentId: '11111111-1111-1111-1111-111111111111' });
            const created = await drain('immediate_response');
            const sessionId = (created.data as { sessionId: string }).sessionId;

            // Turn 1 → runs the slow tool (its toolCall chunk is emitted inline; it is not gated).
            client.sendAction({ action: 'send_message', requestId: 'turn-1', sessionId, message: 'fetch the report' });
            const chunk = await drain('stream_chunk');
            expect(chunk.requestId).toBe('turn-1');
            await slow.entered;

            // Cancel turn 1 while it sits in the tool. The slot frees immediately; the turn does not.
            client.sendAction({ action: 'cancel', requestId: 'turn-1' });
            const cancelled = await drain('cancelled');
            expect(cancelled.requestId).toBe('turn-1');
            expect(cancelled.status).toBe(499);

            // Turn 2 on the SAME session parks on the gated write tool.
            client.sendAction({ action: 'send_message', requestId: 'turn-2', sessionId, message: 'delete record 42' });
            const parked = await drain('write_confirmation_required');
            expect(parked.requestId).toBe('turn-2');

            // Only NOW does turn 1's tool return, dragging its teardown in behind turn 2's park.
            slow.release();
            // Let turn 1 finish unwinding. These are event-loop rounds, not a sleep: turn 1's
            // path from the resolved tool to its `finally` is pure microtask work with no I/O,
            // so this is independent of wall-clock and of how loaded the machine is.
            for (let i = 0; i < 20; i++) await new Promise((resolve) => setImmediate(resolve));

            // Turn 2's confirmation must still be there — this is the regression.
            client.sendAction({ action: 'confirm_tool_action', requestId: 'cf', sessionId, approved: true });
            const done = await drain('eventual_response');
            expect(done.requestId).toBe('turn-2');
            expect(done.status).toBe(200);
            expect(JSON.stringify(done.data)).toContain('Record 42 is gone.');

            // Never NO_PENDING_CONFIRMATION: turn 1's teardown did not touch turn 2's park.
            const codes = all.filter((e) => e.type === 'error').map((e) => (e.error as { code: string }).code);
            expect(codes).not.toContain('NO_PENDING_CONFIRMATION');
        } finally {
            await client.close();
        }
    });
});
