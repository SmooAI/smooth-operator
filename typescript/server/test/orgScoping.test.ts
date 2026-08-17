/**
 * Org is the OUTER scope, applied before ownership.
 *
 * The gap this closes: `mayRead` returned true for ANY ownerless conversation
 * (deliberate — it keeps anonymous / emailless / legacy sessions reachable, see
 * PRs #308/#309) and never consulted org. So an ownerless conversation belonging
 * to another org was readable by anyone holding its id — authorization resting on
 * an unguessable UUID, which leaks through logs, referrers and screenshots.
 */
import { describe, expect, it } from 'vitest';

import { MockLlmProvider } from '@smooai/smooth-operator-core';

import type { AccessContext } from '../src/auth.js';
import { FrameDispatcher } from '../src/frameDispatcher.js';
import type { Frame } from '../src/protocol.js';
import { InMemorySessionStore } from '../src/sessionStore.js';

describe('conversation org scoping', () => {
    it('records the owning org at creation and never re-homes it on resume', async () => {
        const store = new InMemorySessionStore();
        // No userEmail → an OWNERLESS conversation: the case ownership cannot block,
        // so only the org check can. An owned conversation here would pass even with
        // the org check removed, proving nothing.
        const created = await store.createSession('agent', undefined, undefined, undefined, 'org-a');
        expect(created.orgId).toBe('org-a');
        expect(created.userEmail).toBeUndefined();

        const conv = await store.getConversation(created.conversationId);
        expect(conv?.orgId).toBe('org-a');

        // A resume from another org must not rewrite the conversation's org.
        const resumed = await store.createSession('agent', undefined, undefined, created.conversationId, 'org-b');
        expect(resumed.conversationId).toBe(created.conversationId);
        expect(resumed.orgId, 'a resume must inherit the original org, never the resumer’s').toBe('org-a');
    });

    it('leaves the org unrecorded when the caller has none', async () => {
        // Rows created before org capture carry no org; they must not be forced into
        // a bogus one, and the dispatcher falls through to ownership for them.
        const store = new InMemorySessionStore();
        const created = await store.createSession('agent', undefined, 'a@example.com');
        expect(created.orgId).toBeUndefined();
        expect((await store.getConversation(created.conversationId))?.orgId).toBeUndefined();
    });
});

/**
 * The gate itself, driven through the dispatcher — the store tests above only prove
 * the org is recorded, not that anything checks it.
 */
describe('the dispatcher gate checks org before ownership', () => {
    /** An authenticated principal in `org`, optionally with no email claim. */
    function principal(org: string, email?: string): AccessContext {
        return {
            principal: { sub: 'sub', org, role: 'basic', groups: [], ...(email ? { email } : {}) },
            isAnonymous: false,
            authEnabled: true,
        };
    }

    function connect(store: InMemorySessionStore, access: AccessContext) {
        const dispatcher = new FrameDispatcher({ store, chatClient: new MockLlmProvider(), access });
        const sink: Frame[] = [];
        return { sink, dispatch: (f: Record<string, unknown>) => dispatcher.dispatch(JSON.stringify(f), (x) => sink.push(x)) };
    }

    it('hides another org’s OWNERLESS conversation, which ownership alone cannot', async () => {
        const store = new InMemorySessionStore();

        // org-a, emailless → an ownerless conversation. Ownership can never block a
        // read of this, so the org check is the only thing that can.
        const a = connect(store, principal('org-a'));
        await a.dispatch({ type: 'action', action: 'create_conversation_session', requestId: 'r1', agentId: 'agent' });
        const conversationId = (a.sink[0]!.data as { conversationId: string }).conversationId;
        expect(conversationId).toBeTruthy();

        // org-b resuming that id must get a FRESH conversation, indistinguishable from
        // having passed an id that never existed.
        const b = connect(store, principal('org-b'));
        await b.dispatch({ type: 'action', action: 'create_conversation_session', requestId: 'r2', agentId: 'agent', conversationId });
        const bConversationId = (b.sink[0]!.data as { conversationId: string }).conversationId;
        expect(bConversationId, 'another org must not be bound to this conversation').not.toBe(conversationId);
    });

    it('lets the SAME org resume its own ownerless conversation', async () => {
        // The behaviour the ownerless rule exists to protect must survive.
        const store = new InMemorySessionStore();
        const a = connect(store, principal('org-a'));
        await a.dispatch({ type: 'action', action: 'create_conversation_session', requestId: 'r1', agentId: 'agent' });
        const conversationId = (a.sink[0]!.data as { conversationId: string }).conversationId;

        const again = connect(store, principal('org-a'));
        await again.dispatch({ type: 'action', action: 'create_conversation_session', requestId: 'r2', agentId: 'agent', conversationId });
        const resumedId = (again.sink[0]!.data as { conversationId: string }).conversationId;
        expect(resumedId, 'same org must keep its own conversation').toBe(conversationId);
    });
});
