/**
 * `get_conversation_messages` action — dispatcher-level parity with the merged Go
 * reference (`go/server/get_messages_test.go`) and the Rust
 * `handle_get_conversation_messages`. Pearl th-75eda5.
 *
 * Drives {@link FrameDispatcher.dispatch} directly (no socket), asserting the wire
 * shape of `spec/actions/get-messages.schema.json`: newest-first `messages`
 * (`id`, `direction`, `content.text`, `createdAt`) plus `hasMore`, with `limit`
 * (1..100, default 50) and an optional ISO 8601 `before` cursor.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { describe, expect, it } from 'vitest';

import { FrameDispatcher } from '../src/frameDispatcher.js';
import type { Frame } from '../src/protocol.js';
import { InMemorySessionStore } from '../src/sessionStore.js';

/** A dispatcher over a fresh store with one session; returns both plus a sink-bound dispatch. */
async function setup() {
    const store = new InMemorySessionStore();
    const session = await store.createSession('agent-msgs', 'Alice', 'alice@example.com');
    const dispatcher = new FrameDispatcher({ store, chatClient: new MockLlmProvider() });
    const sink: Frame[] = [];
    const dispatch = (frame: Record<string, unknown>) => dispatcher.dispatch(JSON.stringify(frame), (f) => sink.push(f));
    return { store, session, sink, dispatch };
}

/** The single emitted event's `data` payload, asserting it was an immediate_response. */
function payload(sink: Frame[]): { messages: Record<string, unknown>[]; hasMore: boolean } {
    expect(sink).toHaveLength(1);
    expect(sink[0]!.type).toBe('immediate_response');
    return sink[0]!.data as { messages: Record<string, unknown>[]; hasMore: boolean };
}

const text = (m: Record<string, unknown>): unknown => (m.content as Record<string, unknown>).text;

describe('get_conversation_messages action', () => {
    it('returns a conversation newest-first in the documented shape', async () => {
        const { store, session, sink, dispatch } = await setup();
        await store.appendMessage(session.conversationId, 'inbound', 'first');
        await store.appendMessage(session.conversationId, 'outbound', 'second');

        await dispatch({ action: 'get_conversation_messages', requestId: 'gm-1', sessionId: session.sessionId });

        const data = payload(sink);
        expect(sink[0]!.requestId).toBe('gm-1');
        expect(data.hasMore).toBe(false);
        expect(data.messages).toHaveLength(2);
        // Newest-first: the outbound "second" leads.
        expect(data.messages.map((m) => m.direction)).toEqual(['outbound', 'inbound']);
        expect(text(data.messages[0]!)).toBe('second');
        expect(data.messages[0]!.id).toEqual(expect.any(String));
        expect(Date.parse(data.messages[0]!.createdAt as string)).not.toBeNaN();
        // No stray fields on the wire beyond the contract's four.
        expect(Object.keys(data.messages[0]!).sort()).toEqual(['content', 'createdAt', 'direction', 'id']);
    });

    it('an unknown sessionId is SESSION_NOT_FOUND', async () => {
        const { sink, dispatch } = await setup();

        await dispatch({ action: 'get_conversation_messages', requestId: 'gm-1', sessionId: 'nope' });

        expect(sink).toHaveLength(1);
        expect(sink[0]!.type).toBe('error');
        expect((sink[0]!.error as Record<string, unknown>).code).toBe('SESSION_NOT_FOUND');
    });

    it('a missing sessionId is a VALIDATION_ERROR', async () => {
        const { sink, dispatch } = await setup();

        await dispatch({ action: 'get_conversation_messages', requestId: 'gm-1' });

        expect(sink).toHaveLength(1);
        expect(sink[0]!.type).toBe('error');
        expect((sink[0]!.error as Record<string, unknown>).code).toBe('VALIDATION_ERROR');
    });

    it('a limit below the message count returns exactly `limit` and sets hasMore', async () => {
        const { store, session, sink, dispatch } = await setup();
        for (const t of ['m1', 'm2', 'm3', 'm4']) await store.appendMessage(session.conversationId, 'inbound', t);

        await dispatch({ action: 'get_conversation_messages', requestId: 'gm-1', sessionId: session.sessionId, limit: 2 });

        const data = payload(sink);
        expect(data.messages).toHaveLength(2);
        expect(data.hasMore).toBe(true);
        // Newest-first, so the page starts at the last-appended message.
        expect(data.messages.map(text)).toEqual(['m4', 'm3']);
    });

    it('a `before` cursor returns only strictly-older messages', async () => {
        const { store, session, sink, dispatch } = await setup();
        const older = await store.appendMessage(session.conversationId, 'inbound', 'older');
        const newer = await store.appendMessage(session.conversationId, 'outbound', 'newer');
        // Back-to-back appends can land on the same millisecond; stamp explicitly so the
        // cursor boundary is deterministic (the Go reference hit the same flake).
        older.createdAt = '2026-07-18T10:00:00.000Z';
        newer.createdAt = '2026-07-18T11:00:00.000Z';

        await dispatch({
            action: 'get_conversation_messages',
            requestId: 'gm-1',
            sessionId: session.sessionId,
            before: '2026-07-18T11:00:00.000Z',
        });

        const data = payload(sink);
        expect(data.messages.map(text)).toEqual(['older']);
        expect(data.hasMore).toBe(false);
    });

    it('an unparseable `before` is a VALIDATION_ERROR', async () => {
        const { session, sink, dispatch } = await setup();

        await dispatch({ action: 'get_conversation_messages', requestId: 'gm-1', sessionId: session.sessionId, before: 'not-a-date' });

        expect(sink).toHaveLength(1);
        expect(sink[0]!.type).toBe('error');
        expect((sink[0]!.error as Record<string, unknown>).code).toBe('VALIDATION_ERROR');
    });
});
