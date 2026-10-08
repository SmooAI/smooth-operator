/**
 * `list_conversations` paging + search (SMOODEV-3744) — the TS port of
 * `rust/smooth-operator-server/tests/list_conversations_paging.rs` and the unit
 * tests at the bottom of `rust/smooth-operator/src/adapter.rs`. Test names follow
 * their Rust counterparts so parity gaps are visible.
 *
 * The sidebar used to return only the newest `limit` rows with no way to reach
 * older ones. These pin the keyset `cursor` / `nextCursor` and the server-side
 * `query`:
 *
 * - pages are disjoint and, together, exactly the unpaged listing;
 * - ties on `updatedAt` are broken by id, so equal timestamps neither repeat nor
 *   vanish across a page boundary (including sub-millisecond ties a Postgres
 *   `TIMESTAMPTZ` keeps — see `postgres-store.test.ts` for the pushdown path);
 * - a conversation bumped mid-paging is never returned twice, and every untouched
 *   one is still returned (the bumped one moves above the cursor);
 * - a search narrows the caller's scope and never widens it.
 *
 * TS has ONE listing path per store (no Rust-style owned fast path vs scan): the
 * in-memory store goes through the dispatcher's generic pager, the Postgres store
 * pushes the same query down into SQL. Where Rust tests both of its paths, the TS
 * port tests the scoped (authenticated) and the unscoped (auth-disabled) flavors.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { AccessContext } from '../src/auth.js';
import { ANONYMOUS_ACCESS } from '../src/auth.js';
import {
    compareConversationKeys,
    type ConversationPageQuery,
    decodeConversationCursor,
    encodeConversationCursor,
    keyPrecedes,
    matchesConversationSearch,
    normalizeConversationSearch,
    pageConversationSummaries,
} from '../src/conversationPaging.js';
import { FrameDispatcher } from '../src/frameDispatcher.js';
import type { Frame } from '../src/protocol.js';
import { InMemorySessionStore, type ConversationSummary } from '../src/sessionStore.js';
// The client SDK's spec validator (Node-only), to hold the produced frames to the schema.
import { ProtocolValidator } from '../../src/validate.js';

const ME = 'me@smoo.ai';

// ---------------------------------------------------------------------------
// adapter.rs unit-test parity: cursor, ordering, search
// ---------------------------------------------------------------------------

describe('conversation keyset primitives (adapter.rs parity)', () => {
    it('cursor_round_trips_at_full_precision', () => {
        const k = { updatedAt: '2026-10-08T14:30:00.123456789Z', conversationId: '3f2a0c4e-0000-4000-8000-000000000001' };
        expect(decodeConversationCursor(encodeConversationCursor(k))).toEqual(k);
        const whole = { updatedAt: '2026-10-08T14:30:00Z', conversationId: 'a' };
        expect(decodeConversationCursor(encodeConversationCursor(whole))).toEqual(whole);
    });

    it('encodes unpadded base64url of `<updatedAt>|<id>`, decoding the spec fixture cursor', () => {
        const cursor = encodeConversationCursor({ updatedAt: '2026-10-08T14:30:00.123456Z', conversationId: '33333333-3333-3333-3333-333333333333' });
        // The exact cursor in spec/conformance/fixtures.json (list_conversations_response).
        expect(cursor).toBe('MjAyNi0xMC0wOFQxNDozMDowMC4xMjM0NTZafDMzMzMzMzMzLTMzMzMtMzMzMy0zMzMzLTMzMzMzMzMzMzMzMw');
        expect(cursor).not.toMatch(/[=+/]/);
    });

    it('decodes any RFC 3339 timestamp, including offsets and nanoseconds', () => {
        for (const ts of ['2026-10-08T16:30:00.123456789+02:00', '2026-10-08t14:30:00z', '2026-10-08T14:30:00.5-05:30']) {
            const cursor = Buffer.from(`${ts}|id-1`, 'utf8').toString('base64url');
            expect(decodeConversationCursor(cursor), ts).toEqual({ updatedAt: ts, conversationId: 'id-1' });
        }
    });

    it('foreign_cursors_do_not_decode', () => {
        for (const bad of [
            '',
            'not base64!',
            'bm9waXBl', // "nopipe"
            'MjAyNi0xMC0wOFQwMDowMDowMFp8', // "2026-10-08T00:00:00Z|" — empty id
            Buffer.from('yesterday|id-1').toString('base64url'), // bad timestamp
            Buffer.from('2026-13-40T00:00:00Z|id-1').toString('base64url'), // impossible date
            'MjAyNi0xMC0wOFQwMDowMDowMFp8YQ==', // padded — not what we issue
        ]) {
            expect(decodeConversationCursor(bad), bad).toBeUndefined();
        }
    });

    it('precedes_is_newest_first_then_id_descending', () => {
        const k = { updatedAt: '2026-10-08T00:01:40Z', conversationId: 'm' };
        expect(keyPrecedes(k, '2026-10-08T00:01:39Z', 'z'), 'older sorts after').toBe(true);
        expect(keyPrecedes(k, '2026-10-08T00:01:41Z', 'a'), 'newer sorts before').toBe(false);
        expect(keyPrecedes(k, '2026-10-08T00:01:40Z', 'a'), 'same time, smaller id sorts after').toBe(true);
        expect(keyPrecedes(k, '2026-10-08T00:01:40Z', 'm'), 'the key itself is not after itself').toBe(false);
        expect(keyPrecedes(k, '2026-10-08T00:01:40Z', 'z'), 'same time, larger id sorts before').toBe(false);
        // Instants, not strings: the same moment in another offset / precision is a tie.
        expect(keyPrecedes(k, '2026-10-08T02:01:40.000+02:00', 'a')).toBe(true);
        expect(keyPrecedes(k, '2026-10-08T02:01:40.000+02:00', 'm')).toBe(false);
        // Sub-millisecond precision is honoured, not rounded to the JS Date's ms.
        const micro = { updatedAt: '2026-10-08T00:00:00.123456Z', conversationId: 'm' };
        expect(keyPrecedes(micro, '2026-10-08T00:00:00.123455Z', 'z')).toBe(true);
        expect(keyPrecedes(micro, '2026-10-08T00:00:00.123457Z', 'a')).toBe(false);
        expect(compareConversationKeys({ updatedAt: '2026-10-08T00:00:00.1234561Z', conversationId: 'a' }, micro), 'nanosecond digits count too').toBeGreaterThan(0);
    });

    it('search_matches_meaningful_name_or_first_message_case_insensitively', () => {
        expect(normalizeConversationSearch('  ACME ')).toBe('ACME');
        expect(matchesConversationSearch('ACME', 'where is the acme invoice')).toBe(true);
        expect(matchesConversationSearch('ACME', undefined), 'no first message, nothing to match').toBe(false);
        expect(matchesConversationSearch('ACME', 'nothing here')).toBe(false);
        expect(normalizeConversationSearch('   ')).toBeUndefined();
        expect(normalizeConversationSearch(undefined)).toBeUndefined();
        expect(matchesConversationSearch(undefined, undefined), 'no search admits everything').toBe(true);
    });

    it('pages sub-millisecond ties without dropping or repeating a row', () => {
        // Four rows inside ONE millisecond — a JS Date would see a four-way tie and a
        // ms-precision cursor would drop or repeat across the page boundary.
        const rows: ConversationSummary[] = ['a', 'b', 'c', 'd'].map((id, i) => ({
            conversationId: id,
            updatedAt: '2026-10-08T00:00:00.123Z',
            updatedAtExact: `2026-10-08T00:00:00.12345${i}Z`,
            messageCount: 1,
        }));
        const expected = ['d', 'c', 'b', 'a']; // newest (largest µs) first
        const seen: string[] = [];
        let after: ConversationPageQuery['after'];
        for (let i = 0; i < 10; i++) {
            const page = pageConversationSummaries(rows, { limit: 2, after });
            if (page.length === 0) break;
            seen.push(...page.map((r) => r.conversationId));
            const last = page[page.length - 1]!;
            after = decodeConversationCursor(encodeConversationCursor({ updatedAt: last.updatedAtExact!, conversationId: last.conversationId }));
        }
        expect(seen).toEqual(expected);
    });
});

// ---------------------------------------------------------------------------
// list_conversations_paging.rs parity: the dispatcher over a seeded world
// ---------------------------------------------------------------------------

/** An authenticated principal on an auth-ENABLED server. */
function principal(email: string | undefined): AccessContext {
    return {
        principal: { sub: 'user-sub', org: 'acme', role: 'basic', groups: [], ...(email ? { email } : {}) },
        isAnonymous: false,
        authEnabled: true,
    };
}

const BASE = Date.parse('2026-10-08T12:00:00.000Z');
/** `secs` before the fixed test clock. */
const ago = (secs: number) => BASE - secs * 1000;

/** Freeze `Date` at `ms` (timers stay real). */
function at(ms: number): void {
    vi.setSystemTime(ms);
}

/**
 * A non-empty conversation last updated at `ts`, owned by `owner` (undefined = the
 * machine-made, ownerless case), whose first inbound message is `first`.
 */
async function seed(store: InMemorySessionStore, ts: number, owner: string | undefined, first: string): Promise<string> {
    at(ts);
    const s = await store.createSession('agent-1', undefined, owner, undefined, 'acme');
    await store.appendMessage(s.conversationId, 'inbound', first);
    await store.appendMessage(s.conversationId, 'outbound', 'on it');
    return s.conversationId;
}

/** Bump a conversation's `updatedAt` to `ts` the way a live turn does. */
async function bump(store: InMemorySessionStore, id: string, ts: number): Promise<void> {
    at(ts);
    await store.appendMessage(id, 'inbound', 'one more thing');
}

interface World {
    store: InMemorySessionStore;
    /** Mine, newest first (ties by id descending). */
    mine: string[];
    /** The one of mine whose first message mentions Acme. */
    acme: string;
    theirs: string;
    machine: string;
}

async function world(): Promise<World> {
    const store = new InMemorySessionStore();
    const mine: string[] = [];
    let acme = '';
    // Seven of mine; three share one timestamp to force the id tiebreak across a
    // page boundary at limit 3.
    const tied = ago(400);
    const stamps = [ago(10), ago(20), tied, tied, tied, ago(500), ago(600)];
    for (const [i, ts] of stamps.entries()) {
        const first = i === 5 ? 'Where is the Acme invoice?' : 'status update';
        const id = await seed(store, ts, ME, first);
        if (i === 5) acme = id;
        mine.push(id);
    }
    const theirs = await seed(store, ago(15), 'tara@smoo.ai', 'acme renewal');
    const machine = await seed(store, ago(25), undefined, 'ACME widget chat');
    const keyed = mine.map((id, i) => ({ id, ts: stamps[i]! }));
    keyed.sort((a, b) => b.ts - a.ts || (b.id < a.id ? -1 : b.id > a.id ? 1 : 0));
    at(BASE);
    return { store, mine: keyed.map((k) => k.id), acme, theirs, machine };
}

/** One `list_conversations` round trip; returns the single event it produced. */
async function list(store: InMemorySessionStore, access: AccessContext, args: Record<string, unknown>): Promise<Frame> {
    const dispatcher = new FrameDispatcher({ store, chatClient: new MockLlmProvider(), access });
    const sink: Frame[] = [];
    await dispatcher.dispatch(JSON.stringify({ action: 'list_conversations', requestId: 'lc', ...args }), (f) => sink.push(f));
    expect(sink, 'exactly one event').toHaveLength(1);
    return sink[0]!;
}

interface Page {
    conversations: { conversationId: string; title: string; updatedAt: string; messageCount: number }[];
    nextCursor: string | null;
    hasMore: boolean;
}
const data = (ev: Frame) => ev.data as unknown as Page;
const ids = (ev: Frame) => data(ev).conversations.map((c) => c.conversationId);

/** Page through everything with `limit`, returning each page's ids. */
async function allPages(store: InMemorySessionStore, access: AccessContext, limit: number, query?: string): Promise<string[][]> {
    const pages: string[][] = [];
    let cursor: string | null = null;
    for (let guard = 0; guard < 50; guard++) {
        const ev = await list(store, access, { limit, ...(cursor ? { cursor } : {}), ...(query !== undefined ? { query } : {}) });
        expect(ev.type, JSON.stringify(ev)).toBe('immediate_response');
        const page = data(ev);
        // nextCursor is ALWAYS present on the wire (explicit null), non-null iff hasMore.
        expect(page).toHaveProperty('nextCursor');
        expect(page.nextCursor !== null).toBe(page.hasMore);
        pages.push(ids(ev));
        if (!page.hasMore) return pages;
        cursor = page.nextCursor;
    }
    throw new Error('paging never terminated');
}

/** Continue paging from `cursor` (limit 3) to the end. */
async function allPagesFrom(store: InMemorySessionStore, access: AccessContext, cursor: string): Promise<string[]> {
    const out: string[] = [];
    let next: string | null = cursor;
    while (next) {
        const ev = await list(store, access, { limit: 3, cursor: next });
        out.push(...ids(ev));
        next = data(ev).nextCursor;
    }
    return out;
}

const me = () => principal(ME);
const UNSCOPED = ANONYMOUS_ACCESS; // auth disabled: single-tenant local/dev, every conversation

describe('list_conversations paging + search (list_conversations_paging.rs parity)', () => {
    beforeEach(() => {
        vi.useFakeTimers({ toFake: ['Date'] });
        at(BASE);
    });
    afterEach(() => {
        vi.useRealTimers();
    });

    it('owned_pages_are_disjoint_and_complete', async () => {
        const w = await world();
        const pages = await allPages(w.store, me(), 3);
        expect(pages.map((p) => p.length)).toEqual([3, 3, 1]);
        expect(pages.flat(), 'pages concatenate to the full listing, in order').toEqual(w.mine);
    });

    it('scan_pages_are_disjoint_and_complete (unscoped, auth disabled)', async () => {
        const w = await world();
        const unpaged = await list(w.store, UNSCOPED, { limit: 100 });
        const pages = await allPages(w.store, UNSCOPED, 2);
        expect(pages.flat()).toEqual(ids(unpaged));
        expect(new Set(pages.flat()).size).toBe(pages.flat().length);
        // Unscoped sees everything, including the ownerless widget chat.
        expect(pages.flat()).toContain(w.machine);
        expect(pages.flat()).toHaveLength(9);
    });

    it('a_page_the_size_of_the_rest_reports_no_more', async () => {
        const w = await world();
        const ev = await list(w.store, me(), { limit: 7 });
        expect(ids(ev)).toEqual(w.mine);
        expect(data(ev).hasMore).toBe(false);
        expect(data(ev).nextCursor).toBeNull();
    });

    it('no_cursor_is_the_old_first_page', async () => {
        const w = await world();
        const ev = await list(w.store, me(), {});
        expect(ids(ev), 'default limit 50 returns every owned row').toEqual(w.mine);
        const blank = await list(w.store, me(), { limit: 2, cursor: '' });
        expect(ids(blank), 'a blank cursor is no cursor').toEqual(w.mine.slice(0, 2));
        expect(data(blank).hasMore).toBe(true);
    });

    /**
     * Bump one row on the NEXT page and one on the page already read, between
     * pages. Neither may be returned twice; every untouched row must still come
     * back; the bumped next-page row moves above the cursor and is not returned by
     * the remaining pages (it heads a fresh first page instead).
     */
    it('owned_paging_survives_concurrent_updates', async () => {
        const w = await world();
        const first = await list(w.store, me(), { limit: 3 });
        const page1 = ids(first);
        const cursor = data(first).nextCursor;
        expect(cursor).not.toBeNull();

        const bumpedAhead = w.mine[4]!; // would have been on page 2
        const bumpedBehind = page1[1]!; // already returned
        await bump(w.store, bumpedAhead, BASE + 1000);
        await bump(w.store, bumpedBehind, BASE + 2000);

        const rest = await allPagesFrom(w.store, me(), cursor!);
        const seen = [...page1, ...rest];
        expect(new Set(seen).size, `no row returned twice: ${seen}`).toBe(seen.length);
        for (const id of w.mine) {
            if (id !== bumpedAhead) expect(seen, `untouched row ${id} was dropped`).toContain(id);
        }
        expect(rest, 'a row bumped above the cursor is not on later pages').not.toContain(bumpedAhead);

        const fresh = await list(w.store, me(), { limit: 1 });
        expect(ids(fresh), 'the latest bump heads a fresh first page').toEqual([bumpedBehind]);
    });

    it('scan_paging_survives_concurrent_updates (unscoped)', async () => {
        const w = await world();
        const first = await list(w.store, UNSCOPED, { limit: 3 });
        const cursor = data(first).nextCursor!;
        await bump(w.store, data(first).conversations[1]!.conversationId, BASE + 1000);
        const rest = await allPagesFrom(w.store, UNSCOPED, cursor);
        const seen = [...ids(first), ...rest];
        expect(new Set(seen).size, `no row returned twice: ${seen}`).toBe(seen.length);
        for (const id of [...w.mine, w.theirs, w.machine]) expect(seen, `row ${id} was dropped`).toContain(id);
    });

    it('owned_search_matches_my_titles_and_never_widens_scope', async () => {
        const w = await world();
        // "acme" matches my first message, Tara's chat and the ownerless widget chat;
        // only mine is listed — the TS scoped listing excludes ownerless rows.
        const ev = await list(w.store, me(), { query: '  AcMe ' });
        expect(ids(ev)).toEqual([w.acme]);
        expect(data(ev).conversations[0]!.title).toBe('Where is the Acme invoice?');
        expect(data(ev).hasMore).toBe(false);
        expect(data(ev).nextCursor).toBeNull();
    });

    it('scan_search_never_returns_another_members_chat', async () => {
        const w = await world();
        const got = ids(await list(w.store, me(), { query: 'acme' }));
        expect(got).toContain(w.acme);
        expect(got, "another member's matching chat leaked").not.toContain(w.theirs);
        expect(got, 'an ownerless matching chat leaked into a scoped listing').not.toContain(w.machine);
    });

    it('a blank query is no filter', async () => {
        const w = await world();
        expect(ids(await list(w.store, me(), { query: '   ' }))).toEqual(w.mine);
    });

    it('search_matches_a_meaningful_name_but_not_the_default_placeholder', async () => {
        // The TS stores keep no conversation name: the sidebar title is the first
        // inbound message, else the placeholder `Conversation <id>`. The search covers
        // what the sidebar shows — never the placeholder.
        const w = await world();
        const untitled = await seed(w.store, ago(700), ME, '');
        const page = await list(w.store, me(), {});
        const placeholder = data(page).conversations.find((c) => c.conversationId === untitled)!.title;
        expect(placeholder).toBe(`Conversation ${untitled}`);

        expect(ids(await list(w.store, me(), { query: 'conversation' })), 'the placeholder is not a title').toEqual([]);
        expect(ids(await list(w.store, me(), { query: untitled.slice(0, 8) })), 'nor is the id inside it').toEqual([]);
        expect(ids(await list(w.store, me(), { query: 'INVOICE' }))).toEqual([w.acme]);
    });

    it('search_pages_with_a_cursor', async () => {
        const w = await world();
        const pages = await allPages(w.store, me(), 2, 'status');
        const expected = w.mine.filter((id) => id !== w.acme);
        expect(pages.map((p) => p.length)).toEqual([2, 2, 2]);
        expect(pages.flat(), "the six 'status update' rows, in order, once each").toEqual(expected);
    });

    it('an_unknown_cursor_is_a_validation_error', async () => {
        const w = await world();
        for (const cursor of ['not-a-cursor', 'bm9waXBl', '   ', 'MjAyNi0xMC0wOFQwMDowMDowMFp8']) {
            const ev = await list(w.store, me(), { cursor });
            expect(ev.type, cursor).toBe('error');
            expect((ev.error as { code: string }).code).toBe('VALIDATION_ERROR');
            expect(((ev.data as { error: { code: string } }).error).code).toBe('VALIDATION_ERROR');
        }
    });

    it('requests and pages validate against spec/actions/list-conversations.schema.json', async () => {
        const validator = await ProtocolValidator.load(new URL('../../../spec', import.meta.url).pathname);
        const w = await world();
        const first = await list(w.store, me(), { limit: 3, query: 'status' });
        const last = await list(w.store, me(), { limit: 50 });
        for (const ev of [first, last]) {
            const res = validator.validateAt('actions/list-conversations.schema.json#/$defs/Response', ev.data);
            expect(res.errors, JSON.stringify(ev.data)).toEqual([]);
        }
        const next = { action: 'list_conversations', requestId: 'r', limit: 3, cursor: data(first).nextCursor, query: 'status' };
        expect(validator.validateAt('actions/list-conversations.schema.json#/$defs/Request', next).errors).toEqual([]);
    });

    it('an_emailless_principal_pages_nothing', async () => {
        const w = await world();
        const ev = await list(w.store, principal(undefined), { query: 'acme' });
        expect(ids(ev)).toEqual([]);
        expect(data(ev).hasMore).toBe(false);
        expect(data(ev).nextCursor).toBeNull();
    });
});
