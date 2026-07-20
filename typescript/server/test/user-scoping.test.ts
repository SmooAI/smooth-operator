/**
 * Per-user conversation scoping — the cross-user data leak (pearl th-8fe998).
 *
 * Before this, `listConversations` took no user filter, so `list_conversations`
 * returned EVERY user's conversations, and neither the resume path nor
 * `get_conversation_messages` checked ownership. Any authenticated user could
 * enumerate and open anyone else's chats.
 *
 * These tests are written from the ATTACKER's side: user A, authenticated, trying
 * to see or touch user B's conversations by every route the protocol offers —
 * listing, resuming, reading messages, posting turns, and lying about their
 * identity in the create frame. The most important assertion in the file is the
 * existence-oracle one: B's conversation and an id that never existed must produce
 * IDENTICAL payloads, or the difference itself leaks which ids are real.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { describe, expect, it } from 'vitest';

import type { AccessContext } from '../src/auth.js';
import { ANONYMOUS_ACCESS } from '../src/auth.js';
import { FrameDispatcher } from '../src/frameDispatcher.js';
import type { Frame } from '../src/protocol.js';
import { InMemorySessionStore } from '../src/sessionStore.js';

/** An authenticated principal on an auth-ENABLED server, optionally with no email claim. */
function principal(email: string | undefined): AccessContext {
    return {
        principal: { sub: 'user-sub', org: 'acme', role: 'basic', groups: [], ...(email ? { email } : {}) },
        isAnonymous: false,
        authEnabled: true,
    };
}

/** Anonymous on an auth-ENABLED server — a missing/expired/forged token lands here. */
const ANON_UNDER_AUTH: AccessContext = { ...ANONYMOUS_ACCESS, authEnabled: true };

/** A dispatcher bound to `access`, over a SHARED store, plus a sink-bound dispatch. */
function connect(store: InMemorySessionStore, access: AccessContext) {
    const dispatcher = new FrameDispatcher({ store, chatClient: new MockLlmProvider(), access });
    const sink: Frame[] = [];
    const dispatch = (frame: Record<string, unknown>) => dispatcher.dispatch(JSON.stringify(frame), (f) => sink.push(f));
    return { sink, dispatch };
}

/** Create a conversation owned by `email` with one message, via the create action. */
async function seed(store: InMemorySessionStore, email: string, text: string) {
    const { sink, dispatch } = connect(store, principal(email));
    await dispatch({ action: 'create_conversation_session', requestId: 'cs', agentId: 'agent-1' });
    const data = sink[0]!.data as { sessionId: string; conversationId: string };
    await store.appendMessage(data.conversationId, 'inbound', text);
    return data;
}

const conversationIds = (sink: Frame[]): string[] =>
    ((sink[0]!.data as { conversations: { conversationId: string }[] }).conversations ?? []).map((c) => c.conversationId);

describe('per-user conversation scoping (th-8fe998)', () => {
    describe('list_conversations', () => {
        it("returns ONLY the caller's conversations, never another user's", async () => {
            const store = new InMemorySessionStore();
            const a = await seed(store, 'a@example.com', 'alice secret');
            const b = await seed(store, 'b@example.com', 'bob secret');

            const { sink, dispatch } = connect(store, principal('a@example.com'));
            await dispatch({ action: 'list_conversations', requestId: 'lc' });

            expect(conversationIds(sink)).toEqual([a.conversationId]);
            expect(conversationIds(sink)).not.toContain(b.conversationId);
        });

        it('scopes inside the selection, so a small limit still returns the caller\'s own rows', async () => {
            // The filter-after-limit bug: 5 of B's conversations are newer than A's one,
            // so a limit of 2 applied BEFORE the owner filter yields an empty page for A.
            const store = new InMemorySessionStore();
            const a = await seed(store, 'a@example.com', 'alice');
            for (let i = 0; i < 5; i++) await seed(store, 'b@example.com', `bob ${i}`);

            const { sink, dispatch } = connect(store, principal('a@example.com'));
            await dispatch({ action: 'list_conversations', requestId: 'lc', limit: 2 });

            expect(conversationIds(sink)).toEqual([a.conversationId]);
        });

        it('fails closed to an EMPTY list when auth is on but the principal has no email', async () => {
            const store = new InMemorySessionStore();
            await seed(store, 'a@example.com', 'alice');
            await seed(store, 'b@example.com', 'bob');

            for (const access of [principal(undefined), ANON_UNDER_AUTH]) {
                const { sink, dispatch } = connect(store, access);
                await dispatch({ action: 'list_conversations', requestId: 'lc' });
                // Never a silent fall back to unscoped.
                expect(conversationIds(sink)).toEqual([]);
            }
        });

        it('stays UNSCOPED when auth is disabled (single-tenant local/dev)', async () => {
            const store = new InMemorySessionStore();
            const s1 = await store.createSession('agent-1', 'One', 'one@example.com');
            const s2 = await store.createSession('agent-1', 'Two', 'two@example.com');
            await store.appendMessage(s1.conversationId, 'inbound', 'one');
            await store.appendMessage(s2.conversationId, 'inbound', 'two');

            const { sink, dispatch } = connect(store, ANONYMOUS_ACCESS);
            await dispatch({ action: 'list_conversations', requestId: 'lc' });

            expect(conversationIds(sink).sort()).toEqual([s1.conversationId, s2.conversationId].sort());
        });
    });

    describe("reading another user's session", () => {
        it('get_conversation_messages on B\'s session is SESSION_NOT_FOUND', async () => {
            const store = new InMemorySessionStore();
            const b = await seed(store, 'b@example.com', 'bob secret');

            const { sink, dispatch } = connect(store, principal('a@example.com'));
            await dispatch({ action: 'get_conversation_messages', requestId: 'gm', sessionId: b.sessionId });

            expect(sink[0]!.type).toBe('error');
            expect((sink[0]!.error as Record<string, unknown>).code).toBe('SESSION_NOT_FOUND');
        });

        it('get_session on B\'s session is SESSION_NOT_FOUND (no agent/conversation ids leak)', async () => {
            const store = new InMemorySessionStore();
            const b = await seed(store, 'b@example.com', 'bob secret');

            const { sink, dispatch } = connect(store, principal('a@example.com'));
            await dispatch({ action: 'get_session', requestId: 'gs', sessionId: b.sessionId });

            expect(sink[0]!.type).toBe('error');
            expect(JSON.stringify(sink[0]!)).not.toContain(b.conversationId);
        });

        it('send_message into B\'s session is SESSION_NOT_FOUND (no writing to another user\'s chat)', async () => {
            const store = new InMemorySessionStore();
            const b = await seed(store, 'b@example.com', 'bob secret');

            const { sink, dispatch } = connect(store, principal('a@example.com'));
            await dispatch({ action: 'send_message', requestId: 'sm', sessionId: b.sessionId, message: 'injected' });

            expect(sink[0]!.type).toBe('error');
            expect((sink[0]!.error as Record<string, unknown>).code).toBe('SESSION_NOT_FOUND');
            // And nothing was appended to B's conversation.
            expect(await store.listMessages(b.conversationId, 100)).toHaveLength(1);
        });

        it('verify_otp against B\'s session is SESSION_NOT_FOUND (no burning B\'s codes)', async () => {
            const store = new InMemorySessionStore();
            const b = await seed(store, 'b@example.com', 'bob secret');

            const { sink, dispatch } = connect(store, principal('a@example.com'));
            await dispatch({ action: 'verify_otp', requestId: 'vo', sessionId: b.sessionId, code: '123456' });

            expect((sink[0]!.error as Record<string, unknown>).code).toBe('SESSION_NOT_FOUND');
        });

        it('a principal with no email is denied too — emailless never means unrestricted', async () => {
            const store = new InMemorySessionStore();
            const b = await seed(store, 'b@example.com', 'bob secret');

            const { sink, dispatch } = connect(store, principal(undefined));
            await dispatch({ action: 'get_conversation_messages', requestId: 'gm', sessionId: b.sessionId });

            expect((sink[0]!.error as Record<string, unknown>).code).toBe('SESSION_NOT_FOUND');
        });

        it('the owner can still read their OWN session', async () => {
            const store = new InMemorySessionStore();
            const a = await seed(store, 'a@example.com', 'alice');

            const { sink, dispatch } = connect(store, principal('a@example.com'));
            await dispatch({ action: 'get_conversation_messages', requestId: 'gm', sessionId: a.sessionId });

            expect(sink[0]!.type).toBe('immediate_response');
            expect((sink[0]!.data as { messages: unknown[] }).messages).toHaveLength(1);
        });
    });

    describe('no existence oracle', () => {
        it("B's session and an id that never existed produce IDENTICAL responses", async () => {
            const store = new InMemorySessionStore();
            const b = await seed(store, 'b@example.com', 'bob secret');
            // Same shape/length as a real session id, so nothing but existence differs.
            const phantom = '00000000-0000-4000-8000-000000000000';

            for (const action of ['get_conversation_messages', 'get_session', 'send_message'] as const) {
                const real = connect(store, principal('a@example.com'));
                const fake = connect(store, principal('a@example.com'));
                await real.dispatch({ action, requestId: 'r', sessionId: b.sessionId, message: 'x' });
                await fake.dispatch({ action, requestId: 'r', sessionId: phantom, message: 'x' });

                // Byte-identical once the client-supplied id is normalized away: same code,
                // same message template, same frame shape. Any divergence is an oracle for
                // enumerating other users' session ids.
                const norm = (sink: Frame[], id: string) => JSON.stringify(sink).split(id).join('<ID>');
                expect(norm(real.sink, b.sessionId)).toBe(norm(fake.sink, phantom));
            }
        });

        it("resuming B's conversation behaves EXACTLY like resuming an unknown id — a fresh conversation", async () => {
            const store = new InMemorySessionStore();
            const b = await seed(store, 'b@example.com', 'bob secret');
            const phantom = '00000000-0000-4000-8000-000000000000';

            const real = connect(store, principal('a@example.com'));
            const fake = connect(store, principal('a@example.com'));
            await real.dispatch({ action: 'create_conversation_session', requestId: 'cs', agentId: 'agent-1', conversationId: b.conversationId });
            await fake.dispatch({ action: 'create_conversation_session', requestId: 'cs', agentId: 'agent-1', conversationId: phantom });

            const realData = real.sink[0]!.data as { sessionId: string; conversationId: string };
            const fakeData = fake.sink[0]!.data as { sessionId: string; conversationId: string };

            // Both mint a FRESH conversation rather than binding. Erroring on B's id while
            // silently minting for an unknown one would itself confirm B's id is real.
            expect(realData.conversationId).not.toBe(b.conversationId);
            expect(fakeData.conversationId).not.toBe(phantom);
            expect(real.sink[0]!.type).toBe(fake.sink[0]!.type);
            expect(real.sink[0]!.status).toBe(fake.sink[0]!.status);
            // And A's new session sees none of B's history.
            expect(await store.listMessages(realData.conversationId, 100)).toHaveLength(0);
        });
    });

    describe('the principal wins over the frame', () => {
        it("a client claiming someone else's userEmail does NOT get that user's scope", async () => {
            const store = new InMemorySessionStore();
            const b = await seed(store, 'b@example.com', 'bob secret');

            // A authenticates as themselves but lies in the create frame.
            const { sink, dispatch } = connect(store, principal('a@example.com'));
            await dispatch({
                action: 'create_conversation_session',
                requestId: 'cs',
                agentId: 'agent-1',
                userName: 'Bob',
                userEmail: 'b@example.com',
            });
            const spoofed = sink[0]!.data as { conversationId: string };
            await store.appendMessage(spoofed.conversationId, 'inbound', 'spoof attempt');

            // The spoofed session is owned by A (the principal), so it lands in A's list...
            const asA = connect(store, principal('a@example.com'));
            await asA.dispatch({ action: 'list_conversations', requestId: 'lc' });
            expect(conversationIds(asA.sink)).toContain(spoofed.conversationId);

            // ...and A still cannot see B's conversation.
            expect(conversationIds(asA.sink)).not.toContain(b.conversationId);

            // Nor does the spoofed session pollute B's list.
            const asB = connect(store, principal('b@example.com'));
            await asB.dispatch({ action: 'list_conversations', requestId: 'lc' });
            expect(conversationIds(asB.sink)).toEqual([b.conversationId]);
        });

        it('a resume cannot rewrite ownership of a conversation', async () => {
            const store = new InMemorySessionStore();
            const a = await seed(store, 'a@example.com', 'alice');

            // A legitimately resumes their own conversation while claiming B's email.
            const { sink, dispatch } = connect(store, principal('a@example.com'));
            await dispatch({
                action: 'create_conversation_session',
                requestId: 'cs',
                agentId: 'agent-1',
                conversationId: a.conversationId,
                userEmail: 'b@example.com',
            });
            expect((sink[0]!.data as { conversationId: string }).conversationId).toBe(a.conversationId);

            // B still can't see it.
            const asB = connect(store, principal('b@example.com'));
            await asB.dispatch({ action: 'list_conversations', requestId: 'lc' });
            expect(conversationIds(asB.sink)).toEqual([]);
        });
    });
});
