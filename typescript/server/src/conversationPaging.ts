/**
 * Keyset paging + search for `list_conversations` (SMOODEV-3744) — the TS port of
 * `ConversationKey`, `ConversationSummaryQuery` and `sort_conversations_newest_first`
 * in `rust/smooth-operator/src/adapter.rs`.
 *
 * The sidebar order is newest `updatedAt` first, ties broken by `conversationId`
 * descending (plain string compare). A page is the rows strictly AFTER a key in that
 * order — never an offset.
 *
 * Timestamps are compared as INSTANTS at their full stored precision, not as JS
 * `Date`s: a Postgres `TIMESTAMPTZ` keeps microseconds and the Rust server writes
 * them, so two rows inside one millisecond are distinct positions. Rounding them to
 * a `Date`'s millisecond would make a cursor drop or repeat rows at a page boundary.
 * That is why keys carry the timestamp as its original RFC 3339 string.
 */

import type { ConversationSummary } from './sessionStore.js';

/** Default (and fallback for a non-positive / non-numeric) `list_conversations` page size. */
export const DEFAULT_CONVERSATION_PAGE_LIMIT = 50;

/** A position in the sidebar order. `updatedAt` is RFC 3339 at full precision. */
export interface ConversationKey {
    updatedAt: string;
    conversationId: string;
}

/** One page request: at most `limit` rows strictly after `after`, matching `search`. */
export interface ConversationPageQuery {
    limit: number;
    /** Return only rows strictly after this key (the previous page's last row). */
    after?: ConversationKey;
    /**
     * Case-insensitive substring a row's sidebar title source must contain. Always set
     * through {@link normalizeConversationSearch} (trimmed; blank → undefined).
     */
    search?: string;
}

/** An instant as (whole seconds since the epoch, nanoseconds within that second). */
interface Instant {
    seconds: number;
    nanos: number;
}

const RFC3339 = /^(\d{4}-\d{2}-\d{2})[Tt ](\d{2}:\d{2}:\d{2})(?:\.(\d{1,9}))?([Zz]|[+-]\d{2}:\d{2})$/;

/** Parse an RFC 3339 timestamp at full (up to nanosecond) precision; undefined if it isn't one. */
function parseInstant(ts: string): Instant | undefined {
    const m = RFC3339.exec(ts);
    if (!m) return undefined;
    const [, date, time, fraction, offset] = m;
    const zone = offset === 'z' || offset === 'Z' ? 'Z' : offset!;
    const ms = Date.parse(`${date}T${time}${zone}`);
    if (Number.isNaN(ms)) return undefined;
    // Date.parse rolls over impossible dates (Feb 30 → Mar 2) on some engines; reject
    // anything that doesn't round-trip its own calendar fields.
    const [y, mo, d] = date!.split('-').map(Number) as [number, number, number];
    const probe = new Date(Date.UTC(y, mo - 1, d));
    if (probe.getUTCFullYear() !== y || probe.getUTCMonth() !== mo - 1 || probe.getUTCDate() !== d) return undefined;
    const nanos = fraction ? Number(fraction.padEnd(9, '0')) : 0;
    return { seconds: Math.floor(ms / 1000), nanos };
}

/** Instant order of two RFC 3339 timestamps. An unparseable one sorts as the epoch (oldest). */
function compareTimestamps(a: string, b: string): number {
    const x = parseInstant(a) ?? { seconds: 0, nanos: 0 };
    const y = parseInstant(b) ?? { seconds: 0, nanos: 0 };
    return x.seconds - y.seconds || x.nanos - y.nanos;
}

function compareIds(a: string, b: string): number {
    return a < b ? -1 : a > b ? 1 : 0;
}

/**
 * Ascending key order: `(updatedAt, conversationId)`. The sidebar is its REVERSE —
 * sort with `(a, b) => compareConversationKeys(b, a)`.
 */
export function compareConversationKeys(a: ConversationKey, b: ConversationKey): number {
    return compareTimestamps(a.updatedAt, b.updatedAt) || compareIds(a.conversationId, b.conversationId);
}

/**
 * Whether a row keyed `(updatedAt, conversationId)` sorts strictly after `key` in
 * the sidebar order — i.e. belongs to a later page: older, or equally old with a
 * smaller id.
 */
export function keyPrecedes(key: ConversationKey, updatedAt: string, conversationId: string): boolean {
    return compareConversationKeys({ updatedAt, conversationId }, key) < 0;
}

/** The key of a summary: its exact timestamp when the store keeps one, else `updatedAt`. */
export function summaryKey(row: ConversationSummary): ConversationKey {
    return { updatedAt: row.updatedAtExact ?? row.updatedAt, conversationId: row.conversationId };
}

/** The opaque wire cursor for `key`: unpadded base64url of `<updatedAt>|<conversationId>`. */
export function encodeConversationCursor(key: ConversationKey): string {
    return Buffer.from(`${key.updatedAt}|${key.conversationId}`, 'utf8').toString('base64url');
}

const BASE64URL_NO_PAD = /^[A-Za-z0-9_-]*$/;

/**
 * Parse a wire cursor. Undefined when it isn't one this server could have minted:
 * not unpadded base64url, not UTF-8, no `|`, an empty id, or a timestamp that isn't
 * RFC 3339. Accepts any RFC 3339 timestamp (offsets, nanoseconds), as the Rust
 * server does, so cursors interoperate across servers on one database.
 */
export function decodeConversationCursor(cursor: string): ConversationKey | undefined {
    const trimmed = cursor.trim();
    // Node's base64url decoder silently skips junk; validate the alphabet ourselves
    // (and reject the impossible length a single trailing sextet would imply).
    if (!BASE64URL_NO_PAD.test(trimmed) || trimmed.length % 4 === 1) return undefined;
    let raw: string;
    try {
        raw = new TextDecoder('utf-8', { fatal: true }).decode(Buffer.from(trimmed, 'base64url'));
    } catch {
        return undefined;
    }
    const bar = raw.indexOf('|');
    if (bar < 0) return undefined;
    const updatedAt = raw.slice(0, bar);
    const conversationId = raw.slice(bar + 1);
    if (conversationId.length === 0 || parseInstant(updatedAt) === undefined) return undefined;
    return { updatedAt, conversationId };
}

/** Trim a search; blank (or absent) means no filter. */
export function normalizeConversationSearch(query: unknown): string | undefined {
    if (typeof query !== 'string') return undefined;
    const trimmed = query.trim();
    return trimmed.length > 0 ? trimmed : undefined;
}

/**
 * Whether a row whose title source is `firstInboundText` matches `search` (always,
 * with no search). The TS stores keep no conversation name, so the sidebar title is
 * the first inbound message, else the `Conversation <id>` placeholder — and the
 * placeholder is never matched, as Rust never matches its `Session …` default name.
 */
export function matchesConversationSearch(search: string | undefined, firstInboundText: string | undefined): boolean {
    if (search === undefined) return true;
    if (firstInboundText === undefined) return false;
    return firstInboundText.toLowerCase().includes(search.toLowerCase());
}

/** Sort summaries into the sidebar order (newest first, ties by id descending), in place. */
export function sortConversationsNewestFirst(rows: ConversationSummary[]): ConversationSummary[] {
    return rows.sort((a, b) => compareConversationKeys(summaryKey(b), summaryKey(a)));
}

/**
 * One page of an already-scoped set of summaries: drop empties, order newest first,
 * keep rows strictly after `query.after` that match `query.search`, at most
 * `query.limit` of them. The generic path for a store without a pushed-down
 * `listConversationsPage` (the in-memory store); scoping must already have been
 * applied, so a search can only narrow it.
 */
export function pageConversationSummaries(rows: ConversationSummary[], query: ConversationPageQuery): ConversationSummary[] {
    const out: ConversationSummary[] = [];
    for (const row of sortConversationsNewestFirst(rows.filter((r) => r.messageCount > 0))) {
        if (out.length >= query.limit) break;
        if (query.after && !keyPrecedes(query.after, summaryKey(row).updatedAt, row.conversationId)) continue;
        if (!matchesConversationSearch(query.search, row.firstInboundText)) continue;
        out.push(row);
    }
    return out;
}
