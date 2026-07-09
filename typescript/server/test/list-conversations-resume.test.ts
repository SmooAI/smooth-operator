/**
 * `list_conversations` + resume-by-`conversationId` (pearl th-d5b446).
 *
 * Mirrors the merged Rust reference: list rolls up conversations most-recent-first,
 * drops empties, and derives a clean title from the first inbound message; a
 * `create_conversation_session` carrying a known `conversationId` binds to (resumes)
 * that conversation, while an unknown/absent id mints a fresh one.
 *
 * Split into WS-integration cases (real socket, MockLlmProvider) and fast unit cases
 * against the store + the title helper.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { conversationTitle } from '../src/frameDispatcher.js';
import { serve, type RunningServer } from '../src/server.js';
import { InMemorySessionStore } from '../src/sessionStore.js';
import { TestClient } from './wsClient.js';

/** Create a session over the wire; returns its ids. Optionally resume `conversationId`. */
async function createSession(client: TestClient, opts: { requestId: string; conversationId?: string; agentId?: string }): Promise<{ sessionId: string; conversationId: string }> {
    client.sendAction({ action: 'create_conversation_session', ...opts });
    const created = await client.receive();
    expect(created.type).toBe('immediate_response');
    const data = created.data as Record<string, unknown>;
    return { sessionId: data.sessionId as string, conversationId: data.conversationId as string };
}

/** Send one message and drain to its terminal event. */
async function sendMessage(client: TestClient, requestId: string, sessionId: string, message: string): Promise<void> {
    client.sendAction({ action: 'send_message', requestId, sessionId, message });
    await client.receiveUntil('eventual_response');
}

describe('list_conversations + resume over a real WebSocket', () => {
    let server: RunningServer | undefined;

    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('lists non-empty conversations, most-recent first, with previews', async () => {
        server = await serve({ chatClient: new MockLlmProvider().pushText('a').pushText('b') });
        const client = await TestClient.connect(server.url);

        // Two conversations with a turn each, plus one empty (created, never messaged).
        const a = await createSession(client, { requestId: 'cs-a' });
        await sendMessage(client, 'sm-a', a.sessionId, 'How long can I return an item?');
        const empty = await createSession(client, { requestId: 'cs-empty' });
        const c = await createSession(client, { requestId: 'cs-c' });
        await sendMessage(client, 'sm-c', c.sessionId, 'Where is my order?');

        client.sendAction({ action: 'list_conversations', requestId: 'lc' });
        const reply = await client.receive();
        expect(reply.type).toBe('immediate_response');
        expect(reply.status).toBe(200);
        expect(reply.message).toBe('Conversations');

        const conversations = (reply.data as Record<string, unknown>).conversations as Array<Record<string, unknown>>;
        const ids = conversations.map((x) => x.conversationId);
        expect(ids).toContain(a.conversationId);
        expect(ids).toContain(c.conversationId);
        expect(ids).not.toContain(empty.conversationId); // messageCount 0 filtered

        // Each entry has the wire shape + a message-count of 2 (inbound + outbound).
        for (const conv of conversations) {
            expect(typeof conv.conversationId).toBe('string');
            expect(typeof conv.title).toBe('string');
            expect(conv.messageCount).toBe(2);
            expect(new Date(conv.updatedAt as string).toISOString()).toBe(conv.updatedAt); // valid ISO-8601
        }
        // Titles come from the first inbound message.
        const titleOf = (id: string) => conversations.find((x) => x.conversationId === id)!.title;
        expect(titleOf(a.conversationId)).toBe('How long can I return an item?');
        expect(titleOf(c.conversationId)).toBe('Where is my order?');

        // Most-recent-first: sorted descending by updatedAt.
        const stamps = conversations.map((x) => Date.parse(x.updatedAt as string));
        expect(stamps).toEqual([...stamps].sort((x, y) => y - x));

        await client.close();
    });

    it('strips leading markdown/control chars from the title', async () => {
        server = await serve({ chatClient: new MockLlmProvider().pushText('ok') });
        const client = await TestClient.connect(server.url);

        const s = await createSession(client, { requestId: 'cs' });
        await sendMessage(client, 'sm', s.sessionId, '> ### **Please** cancel my subscription');

        client.sendAction({ action: 'list_conversations', requestId: 'lc' });
        const conversations = ((await client.receive()).data as Record<string, unknown>).conversations as Array<Record<string, unknown>>;
        const title = conversations[0]!.title as string;
        expect(title.startsWith('>')).toBe(false);
        expect(title.startsWith('#')).toBe(false);
        expect(title).toBe('Please** cancel my subscription');

        await client.close();
    });

    it('honors the limit (default 50, positive override)', async () => {
        server = await serve({ chatClient: new MockLlmProvider().pushText('1').pushText('2').pushText('3') });
        const client = await TestClient.connect(server.url);

        for (let i = 0; i < 3; i++) {
            const s = await createSession(client, { requestId: `cs-${i}` });
            await sendMessage(client, `sm-${i}`, s.sessionId, `question ${i}`);
        }

        client.sendAction({ action: 'list_conversations', requestId: 'lc', limit: 2 });
        const conversations = ((await client.receive()).data as Record<string, unknown>).conversations as unknown[];
        expect(conversations.length).toBe(2);

        await client.close();
    });

    it('resumes an existing conversation: same id echoed, history replayed', async () => {
        const chat = new MockLlmProvider().pushText('First answer.').pushText('Second answer.');
        server = await serve({ chatClient: chat });
        const client = await TestClient.connect(server.url);

        // Turn one on a fresh conversation.
        const first = await createSession(client, { requestId: 'cs-1' });
        await sendMessage(client, 'sm-1', first.sessionId, 'first question');

        // A NEW session bound to the SAME conversation — id is echoed back, session differs.
        const resumed = await createSession(client, { requestId: 'cs-2', conversationId: first.conversationId });
        expect(resumed.conversationId).toBe(first.conversationId);
        expect(resumed.sessionId).not.toBe(first.sessionId);

        // Turn two on the resumed session sees turn one's history (proves the binding).
        await sendMessage(client, 'sm-2', resumed.sessionId, 'second question');
        expect(chat.callCount).toBe(2);
        const secondCall = (chat.calls[1]!.messages as Array<Record<string, unknown>>).map((m) => m.content);
        expect(secondCall).toContain('first question');
        expect(secondCall).toContain('First answer.');

        await client.close();
    });

    it('an unknown conversationId falls back to a fresh conversation', async () => {
        server = await serve({ chatClient: new MockLlmProvider().pushText('ok') });
        const client = await TestClient.connect(server.url);

        const bogus = 'does-not-exist-0000';
        const s = await createSession(client, { requestId: 'cs', conversationId: bogus });
        expect(s.conversationId).not.toBe(bogus);
        expect(s.conversationId).toMatch(/^[0-9a-f-]{36}$/); // a fresh UUID

        await client.close();
    });
});

describe('InMemorySessionStore: conversations + resume', () => {
    it('rolls up conversations with count, first-inbound preview, and ISO updatedAt', async () => {
        const store = new InMemorySessionStore();
        const s = await store.createSession('agent');
        await store.appendMessage(s.conversationId, 'inbound', 'hi there');
        await store.appendMessage(s.conversationId, 'outbound', 'hello!');

        const [summary] = await store.listConversations();
        expect(summary!.conversationId).toBe(s.conversationId);
        expect(summary!.messageCount).toBe(2);
        expect(summary!.firstInboundText).toBe('hi there');
        expect(new Date(summary!.updatedAt).toISOString()).toBe(summary!.updatedAt);
    });

    it('a fresh (message-less) conversation still lists with count 0 and no preview', async () => {
        const store = new InMemorySessionStore();
        const s = await store.createSession('agent');
        const [summary] = await store.listConversations();
        expect(summary!.conversationId).toBe(s.conversationId);
        expect(summary!.messageCount).toBe(0);
        expect(summary!.firstInboundText).toBeUndefined();
    });

    it('resume reuses the id + keeps history; unknown id mints a fresh conversation', async () => {
        const store = new InMemorySessionStore();
        const first = await store.createSession('agent');
        await store.appendMessage(first.conversationId, 'inbound', 'kept');

        const resumed = await store.createSession('agent', undefined, undefined, first.conversationId);
        expect(resumed.conversationId).toBe(first.conversationId);
        expect(resumed.sessionId).not.toBe(first.sessionId);
        expect(await store.listMessages(first.conversationId, 10)).toHaveLength(1); // history intact

        const unknown = await store.createSession('agent', undefined, undefined, 'nope');
        expect(unknown.conversationId).not.toBe('nope');
        expect(await store.getConversation('nope')).toBeNull();
        expect(await store.getConversation(first.conversationId)).toEqual({ conversationId: first.conversationId });
    });
});

describe('conversationTitle', () => {
    it('strips leading markdown/control noise', () => {
        expect(conversationTitle('> quoted', 'fb')).toBe('quoted');
        expect(conversationTitle('### Heading', 'fb')).toBe('Heading');
        expect(conversationTitle('▎ cursor', 'fb')).toBe('cursor');
        expect(conversationTitle('  * bullet', 'fb')).toBe('bullet');
    });

    it('falls back when there is no usable inbound text', () => {
        expect(conversationTitle(undefined, 'fallback')).toBe('fallback');
        expect(conversationTitle('   ', 'fallback')).toBe('fallback');
        expect(conversationTitle('###', 'fallback')).toBe('fallback');
    });

    it('truncates to 60 chars with an ellipsis', () => {
        const long = 'x'.repeat(70);
        const title = conversationTitle(long, 'fb');
        expect([...title]).toHaveLength(61); // 60 + ellipsis
        expect(title.endsWith('…')).toBe(true);
    });
});
