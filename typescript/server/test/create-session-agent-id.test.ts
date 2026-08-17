import { describe, expect, it } from 'vitest';

import { FrameDispatcher } from '../src/frameDispatcher.js';
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import type { Frame } from '../src/protocol.js';
import { InMemorySessionStore } from '../src/sessionStore.js';

/**
 * `agentId` is REQUIRED by the Request schema, so absent-or-blank is a malformed request —
 * not an agentless session. Asserts BOTH halves: the request is rejected, and NOTHING is
 * persisted. A rejection that still writes a row is the same bug wearing an error message.
 */
describe('create_conversation_session agentId validation', () => {
    for (const [name, agentId] of [
        ['absent', undefined],
        ['empty', ''],
        ['whitespace', '   '],
    ] as const) {
        it(`rejects an ${name} agentId without persisting anything`, async () => {
            const store = new InMemorySessionStore();
            const dispatcher = new FrameDispatcher({ store, chatClient: new MockLlmProvider() });
            const sink: Frame[] = [];

            await dispatcher.dispatch(
                JSON.stringify({ action: 'create_conversation_session', requestId: 'r1', ...(agentId === undefined ? {} : { agentId }) }),
                (f) => sink.push(f),
            );

            expect(sink).toHaveLength(1);
            expect(sink[0]!.type).toBe('error');
            expect((sink[0]!.error as { code: string }).code).toBe('VALIDATION_ERROR');

            // …and no conversation was persisted.
            expect(await store.listConversations(undefined)).toHaveLength(0);
        });
    }

    it('accepts a real agentId', async () => {
        const store = new InMemorySessionStore();
        const dispatcher = new FrameDispatcher({ store, chatClient: new MockLlmProvider() });
        const sink: Frame[] = [];
        await dispatcher.dispatch(JSON.stringify({ action: 'create_conversation_session', requestId: 'r1', agentId: 'agent-1' }), (f) =>
            sink.push(f),
        );
        expect(sink[0]!.type).toBe('immediate_response');
        expect((sink[0]!.data as { agentId: string }).agentId).toBe('agent-1');
    });
});
