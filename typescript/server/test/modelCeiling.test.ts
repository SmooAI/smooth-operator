/**
 * Unit tests for the gateway model-output-ceiling lookup (EPIC th-1cc9fa) — the
 * consumer half of the engine's `max_tokens` clamp. All offline: the `/model/info`
 * fetch is injected as a {@link FetchLike} fake, so no gateway is contacted.
 */
import { describe, expect, it } from 'vitest';

import { createGatewayModelCeilingResolver, extractModelCeilings, type FetchLike } from '../src/modelCeiling.js';

/** A representative LiteLLM `/model/info` payload. */
const SAMPLE = {
    data: [
        { model_name: 'claude-haiku-4-5', model_info: { max_output_tokens: 8192, input_cost_per_token: 0.000001 } },
        { model_name: 'claude-opus-4', model_info: { max_output_tokens: 65536 } },
        { model_name: 'groq-compound', model_info: { max_output_tokens: 8192 } },
    ],
};

/** A `FetchLike` that always resolves the given payload with `ok: true`, counting calls. */
function okFetch(payload: unknown): { fetch: FetchLike; calls: () => number; lastInit: () => unknown } {
    let count = 0;
    let last: unknown;
    const fetch: FetchLike = (_url, init) => {
        count += 1;
        last = init;
        return Promise.resolve({ ok: true, json: () => Promise.resolve(payload) });
    };
    return { fetch, calls: () => count, lastInit: () => last };
}

describe('extractModelCeilings', () => {
    it('maps model_name -> max_output_tokens from a sample payload', () => {
        const map = extractModelCeilings(SAMPLE);
        expect(map.get('claude-haiku-4-5')).toBe(8192);
        expect(map.get('claude-opus-4')).toBe(65536);
        expect(map.get('groq-compound')).toBe(8192);
        expect(map.size).toBe(3);
    });

    it('skips entries with no model_name, no model_info, or a non-positive/non-integer ceiling', () => {
        const map = extractModelCeilings({
            data: [
                { model_info: { max_output_tokens: 4096 } }, // no model_name
                { model_name: 'bare' }, // no model_info
                { model_name: 'zero', model_info: { max_output_tokens: 0 } }, // non-positive
                { model_name: 'floaty', model_info: { max_output_tokens: 1024.5 } }, // non-integer
                { model_name: 'good', model_info: { max_output_tokens: 2048 } },
            ],
        });
        expect(map.size).toBe(1);
        expect(map.get('good')).toBe(2048);
    });

    it('returns an empty map when data is missing or not an array', () => {
        expect(extractModelCeilings({}).size).toBe(0);
        expect(extractModelCeilings({ data: 'nope' }).size).toBe(0);
        expect(extractModelCeilings(null).size).toBe(0);
    });
});

describe('createGatewayModelCeilingResolver', () => {
    it('resolves a known model to its ceiling', async () => {
        const { fetch } = okFetch(SAMPLE);
        const resolve = createGatewayModelCeilingResolver('https://llm.smoo.ai/v1', 'k', fetch);
        expect(await resolve('claude-haiku-4-5')).toBe(8192);
        expect(await resolve('groq-compound')).toBe(8192);
    });

    it('returns undefined for an unknown model (best-effort, no clamp)', async () => {
        const { fetch } = okFetch(SAMPLE);
        const resolve = createGatewayModelCeilingResolver('https://llm.smoo.ai/v1', 'k', fetch);
        expect(await resolve('who-dis')).toBeUndefined();
    });

    it('fetches /model/info at most once and caches the map', async () => {
        const { fetch, calls } = okFetch(SAMPLE);
        const resolve = createGatewayModelCeilingResolver('https://llm.smoo.ai/v1', 'k', fetch);
        await resolve('claude-haiku-4-5');
        await resolve('claude-opus-4');
        await resolve('groq-compound');
        expect(calls()).toBe(1);
    });

    it('hits {gatewayUrl}/model/info with a trimmed base and a bearer header when a key is set', async () => {
        const seen: { url?: string; init?: unknown } = {};
        const fetch: FetchLike = (url, init) => {
            seen.url = url;
            seen.init = init;
            return Promise.resolve({ ok: true, json: () => Promise.resolve(SAMPLE) });
        };
        const resolve = createGatewayModelCeilingResolver('https://llm.smoo.ai/v1/', 'secret', fetch);
        await resolve('claude-haiku-4-5');
        expect(seen.url).toBe('https://llm.smoo.ai/v1/model/info');
        expect(seen.init).toEqual({ headers: { authorization: 'Bearer secret' } });
    });

    it('sends no auth header when no key is provided', async () => {
        const seen: { init?: unknown } = {};
        const fetch: FetchLike = (_url, init) => {
            seen.init = init;
            return Promise.resolve({ ok: true, json: () => Promise.resolve(SAMPLE) });
        };
        const resolve = createGatewayModelCeilingResolver('https://llm.smoo.ai/v1', undefined, fetch);
        await resolve('claude-haiku-4-5');
        expect(seen.init).toBeUndefined();
    });

    it('is best-effort: a non-ok response yields undefined and is not cached (retries next call)', async () => {
        let ok = false;
        let count = 0;
        const fetch: FetchLike = () => {
            count += 1;
            return Promise.resolve({ ok, json: () => Promise.resolve(SAMPLE) });
        };
        const resolve = createGatewayModelCeilingResolver('https://llm.smoo.ai/v1', 'k', fetch);
        expect(await resolve('claude-haiku-4-5')).toBeUndefined(); // first fetch fails
        ok = true; // gateway recovers
        expect(await resolve('claude-haiku-4-5')).toBe(8192); // retried, not stuck on the failure
        expect(count).toBe(2);
    });

    it('is best-effort: a thrown fetch yields undefined', async () => {
        const fetch: FetchLike = () => Promise.reject(new Error('connreset'));
        const resolve = createGatewayModelCeilingResolver('https://llm.smoo.ai/v1', 'k', fetch);
        expect(await resolve('claude-haiku-4-5')).toBeUndefined();
    });

    it('shares a single in-flight fetch across concurrent first lookups', async () => {
        let count = 0;
        const fetch: FetchLike = () => {
            count += 1;
            return new Promise((resolve) => setTimeout(() => resolve({ ok: true, json: () => Promise.resolve(SAMPLE) }), 5));
        };
        const resolve = createGatewayModelCeilingResolver('https://llm.smoo.ai/v1', 'k', fetch);
        const [a, b] = await Promise.all([resolve('claude-haiku-4-5'), resolve('claude-opus-4')]);
        expect(a).toBe(8192);
        expect(b).toBe(65536);
        expect(count).toBe(1);
    });
});
