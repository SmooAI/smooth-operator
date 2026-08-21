/**
 * `supports` — the client render-capability list that gates the ENTIRE Rich
 * Interactions framework — must survive a reconnect.
 *
 * A reconnect IS a resume: the client re-opens the socket and re-issues
 * `create_conversation_session` with the same `conversationId`, which mints a NEW
 * session id on a NEW dispatcher. So while the declared list lived only on the
 * session record, the server forgot it could render cards unless the client
 * re-declared on every single reconnect — and every interaction kind quietly fell
 * back to conversational collection with no error, no event, nothing on the wire to
 * notice. Reconnects are routine (network blips, mobile backgrounding, deploys), so
 * a shipped feature was degrading in the field with no signal (th-13df6d).
 *
 * The list now rides the CONVERSATION, and a resume that OMITS the key inherits it.
 * A frame that DOES declare — `[]` included — replaces the stored set, which is the
 * text-only opt-out and is itself durable.
 *
 * Two dispatchers over one store is the point: a single dispatcher would pass even
 * with the state kept per connection.
 */
import { describe, expect, it } from 'vitest';

import { MockLlmProvider } from '@smooai/smooth-operator-core';

import { FrameDispatcher } from '../src/frameDispatcher.js';
import type { Frame } from '../src/protocol.js';
import { InMemorySessionStore } from '../src/sessionStore.js';

const AGENT = '11111111-1111-1111-1111-111111111111';

describe('declared render capabilities survive a reconnect', () => {
    /** A FRESH dispatcher over the shared store — i.e. a new WebSocket connection. */
    function reconnect(store: InMemorySessionStore) {
        const dispatcher = new FrameDispatcher({ store, chatClient: new MockLlmProvider() });
        return async (frame: Record<string, unknown>): Promise<{ sessionId: string; conversationId: string }> => {
            const sink: Frame[] = [];
            await dispatcher.dispatch(JSON.stringify({ type: 'action', action: 'create_conversation_session', agentId: AGENT, ...frame }), (f) => sink.push(f));
            const data = sink[0]?.data as { sessionId?: string; conversationId?: string } | undefined;
            expect(data?.sessionId, `create_conversation_session failed: ${JSON.stringify(sink[0])}`).toBeTruthy();
            return { sessionId: data!.sessionId!, conversationId: data!.conversationId! };
        };
    }

    /** What the turn reads to decide rich-card vs conversational fallback. */
    async function capabilities(store: InMemorySessionStore, sessionId: string): Promise<string[]> {
        return (await store.getSession(sessionId))?.supports ?? [];
    }

    it('inherits an omitted list on reconnect, and an explicit [] opts out for good', async () => {
        const store = new InMemorySessionStore();

        // Connection 1 declares the capability.
        const first = await reconnect(store)({ requestId: 'req-conn-1', supports: ['identity_form'] });
        expect(await capabilities(store, first.sessionId)).toContain('identity_form');

        // Connection 2 — a reconnect: same conversation, `supports` OMITTED, which is
        // exactly what a widget resuming from its stored conversationId sends. This is
        // where the feature went dark.
        const resumed = await reconnect(store)({ requestId: 'req-conn-2', conversationId: first.conversationId });
        expect(resumed.conversationId, 'the reconnect resumed the same conversation').toBe(first.conversationId);
        expect(resumed.sessionId, 'a reconnect mints a NEW session id — that is the whole problem').not.toBe(first.sessionId);
        expect(await capabilities(store, resumed.sessionId), "a reconnect that omits 'supports' inherits the conversation's declared capabilities").toContain(
            'identity_form',
        );

        // Connection 3 DECLARES `[]` — a text-only channel (SMS, voice) resuming a rich
        // conversation opts out rather than being handed cards it cannot render.
        const textOnly = await reconnect(store)({ requestId: 'req-conn-3', conversationId: first.conversationId, supports: [] });
        expect(await capabilities(store, textOnly.sessionId), "an explicit empty 'supports' declares text-only and never inherits").toEqual([]);

        // Connection 4 omits again: the opt-out must be durable, not a one-session
        // exception that the next reconnect resurrects from a stale record.
        const afterOptOut = await reconnect(store)({ requestId: 'req-conn-4', conversationId: first.conversationId });
        expect(await capabilities(store, afterOptOut.sessionId), 'the text-only declaration replaced the durable record').toEqual([]);
    });

    it('leaves a fresh conversation that declares nothing text-only', async () => {
        // The inherit rule keys on a RESUME; a brand-new conversation with no `supports`
        // is unchanged behavior and must not pick up a neighbour's capabilities.
        const store = new InMemorySessionStore();
        const rich = await reconnect(store)({ requestId: 'r1', supports: ['identity_form'] });
        expect(await capabilities(store, rich.sessionId)).toContain('identity_form');

        const fresh = await reconnect(store)({ requestId: 'r2' });
        expect(fresh.conversationId).not.toBe(rich.conversationId);
        expect(await capabilities(store, fresh.sessionId)).toEqual([]);
    });
});
