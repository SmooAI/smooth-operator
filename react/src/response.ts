/**
 * Defensive readers for the terminal `eventual_response` payload.
 *
 * These mirror the widget's internal helpers: the server's response envelope and
 * citation list are optional and back-compatible, so we tolerate their total
 * absence, non-array shapes, and missing fields rather than letting a slightly
 * different server break rendering.
 */
import type { Citation } from '@smooai/smooth-operator';

/** Pull the final assistant text out of an `eventual_response` data payload. */
export function extractFinalText(response: unknown): string | null {
    if (!response || typeof response !== 'object') return null;
    const r = response as { responseParts?: unknown };
    if (Array.isArray(r.responseParts)) {
        return r.responseParts.filter((p): p is string => typeof p === 'string').join('\n\n');
    }
    return null;
}

/** Pull the grounding {@link Citation}s out of a terminal `eventual_response`'s inner data. */
export function extractCitations(inner: unknown): Citation[] {
    if (!inner || typeof inner !== 'object') return [];
    const raw = (inner as { citations?: unknown }).citations;
    if (!Array.isArray(raw)) return [];
    const out: Citation[] = [];
    for (const c of raw) {
        if (!c || typeof c !== 'object') continue;
        const obj = c as Record<string, unknown>;
        const id = typeof obj.id === 'string' ? obj.id : '';
        const title = typeof obj.title === 'string' ? obj.title : id || 'Source';
        const snippet = typeof obj.snippet === 'string' ? obj.snippet : '';
        const url = typeof obj.url === 'string' && obj.url ? obj.url : undefined;
        const score = typeof obj.score === 'number' ? obj.score : 0;
        out.push({ id, title, snippet, score, url });
    }
    return out;
}

/**
 * Only `http(s)` URLs become anchor hrefs. Mirrors the widget's `safeHttpUrl`
 * XSS guard so a `javascript:`/`data:` citation URL can never become a live link.
 */
export function safeHttpUrl(url: string | undefined): string | undefined {
    if (!url) return undefined;
    try {
        const parsed = new URL(url, 'http://invalid.local');
        if (parsed.protocol === 'http:' || parsed.protocol === 'https:') {
            // Reject the sentinel base — only keep genuinely absolute http(s) URLs.
            if (!/^https?:/i.test(url)) return undefined;
            return url;
        }
    } catch {
        return undefined;
    }
    return undefined;
}
