/**
 * Cancelling a turn PARKED at a write-confirmation (HITL) — parity with the Rust
 * reference and the .NET `CancelUnparkTests` (PR #460).
 *
 * The park awaits a bare deferred from {@link ConfirmationRegistry.register}
 * (`turnRunner.ts` — `const approved = await verdict`) that the turn's `cancelSignal`
 * abort does NOT itself complete. So the `cancel` path must ALSO discard the session's
 * pending confirmation, or the parked turn would linger until the next register /
 * disconnect evicts it. In the TS server that discard is `cancelActiveTurn()` resolving
 * the confirmation on cancel — this test pins that behaviour.
 *
 * Drives the real WS server with a confirm-gated tool + a scripted {@link MockLlmProvider}
 * (offline), parks turn 1 at `write_confirmation_required`, cancels it, then asserts:
 *   1. a terminal `cancelled` (status 499) is emitted for the cancelled turn;
 *   2. the pending confirmation is GONE — a later `confirm_tool_action` yields
 *      `NO_PENDING_CONFIRMATION` (no zombie parked turn to resume);
 *   3. the slot is freed — a NEW `send_message` is accepted (202), not `TURN_IN_PROGRESS`;
 *   4. no stray events leak from the abandoned parked turn (no `eventual_response`, no
 *      `error`; `cancelled` is its terminal and singular event).
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import type { Tool } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { serve, type RunningServer } from '../src/server.js';
import { InMemorySessionStore } from '../src/sessionStore.js';
import { TestClient } from './wsClient.js';

const GATED_TOOL = 'delete_record';

/** The gated write tool. Never actually runs in this test — the turn is cancelled while parked. */
function gatedTool(): Tool {
    return {
        name: GATED_TOOL,
        description: 'Delete a record by id (a state-mutating write).',
        parameters: { type: 'object', properties: { id: { type: 'string' } }, required: ['id'] },
        execute: async (): Promise<string> => 'Record 42 deleted.',
    };
}

/**
 * Turn 1 calls the gated tool (parks). The trailing text is never legitimately consumed
 * by turn 1 (it is cancelled before a second model call) — it is present so a SUBSEQUENT
 * turn always has a response to complete with. Mirrors the .NET script.
 */
function scriptedMock(): MockLlmProvider {
    const mock = new MockLlmProvider();
    mock.pushToolCall('call-1', GATED_TOOL, '{"id": "42"}');
    mock.pushText('done');
    return mock;
}

/** Drive create_conversation_session and return the new session id. */
async function createSession(client: TestClient): Promise<string> {
    client.sendAction({ action: 'create_conversation_session', requestId: 'cs', agentId: '11111111-1111-1111-1111-111111111111' });
    for (;;) {
        const event = await client.receive();
        if (event.type === 'immediate_response') return (event.data as { sessionId: string }).sessionId;
    }
}

describe('cancel-unpark — cancelling a turn parked at a write-confirmation', () => {
    let server: RunningServer | undefined;

    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('discards the pending confirmation and drops the parked turn cleanly', async () => {
        server = await serve({ chatClient: scriptedMock(), store: new InMemorySessionStore(), tools: [gatedTool()], confirmTools: [GATED_TOOL] });
        const client = await TestClient.connect(server.url);
        // Every frame ever received, so we can assert nothing strays from the abandoned turn.
        const all: Record<string, unknown>[] = [];
        const drain = async (type: string): Promise<Record<string, unknown>> => {
            const { terminal, seen } = await client.receiveUntil(type);
            all.push(...seen);
            return terminal;
        };
        try {
            const sessionId = await createSession(client);

            // Turn 1 → parks at the write-confirmation.
            client.sendAction({ action: 'send_message', requestId: 'turn-1', sessionId, message: 'delete record 42' });
            const parked = await drain('write_confirmation_required');
            expect(parked.requestId).toBe('turn-1');

            // Cancel the parked turn. `cancelled` (status 499) is emitted for turn-1.
            client.sendAction({ action: 'cancel', requestId: 'turn-1' });
            const cancelled = await drain('cancelled');
            expect(cancelled.requestId).toBe('turn-1');
            expect(cancelled.status).toBe(499);

            // The pending confirmation is GONE: a confirm_tool_action for the session finds
            // nothing to resolve — no zombie parked turn to approve.
            client.sendAction({ action: 'confirm_tool_action', requestId: 'cf', sessionId, approved: true });
            const confirmReply = await drain('error');
            expect(confirmReply.requestId).toBe('cf');
            expect((confirmReply.error as { code: string }).code).toBe('NO_PENDING_CONFIRMATION');

            // The slot is freed: a NEW send_message is accepted (202), not TURN_IN_PROGRESS.
            client.sendAction({ action: 'send_message', requestId: 'turn-2', sessionId, message: 'hello again' });
            const turn2Terminal = await drain('eventual_response');
            expect(turn2Terminal.requestId).toBe('turn-2');
            expect(turn2Terminal.status).toBe(200);

            // No stray events leaked from the abandoned parked turn. Its only frames are the
            // ack, the park prompt (write_confirmation_required + the deferred gated-tool
            // stream_chunk), and the terminal cancelled — never an eventual_response or error
            // after cancellation, and cancelled is both terminal and singular.
            const turn1 = all.filter((e) => e.requestId === 'turn-1');
            expect(turn1.some((e) => e.type === 'eventual_response')).toBe(false);
            expect(turn1.some((e) => e.type === 'error')).toBe(false);
            expect(turn1.filter((e) => e.type === 'cancelled')).toHaveLength(1);
            expect(turn1[turn1.length - 1].type).toBe('cancelled');
        } finally {
            await client.close();
        }
    });
});
