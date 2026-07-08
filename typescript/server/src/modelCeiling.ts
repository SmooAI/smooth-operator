/**
 * Per-model output-ceiling lookup from the LLM gateway's `/model/info` — the
 * consumer half of the model-output clamp (EPIC th-1cc9fa).
 *
 * The engine (`@smooai/smooth-operator-core`) clamps a turn's `max_tokens` to
 * `min(maxTokens, modelMaxOutput)` but takes no LiteLLM-specific HTTP itself. The
 * server sources the ceiling here and passes it in via `AgentOptions.modelMaxOutput`,
 * mirroring the Rust server's `admin::fetch_model_costs` / `model_output_ceiling` /
 * `map_model_info` split (`rust/smooth-operator-server/src/admin.rs`).
 *
 * **Best-effort**: any gateway/transport/decode error, an unknown model, or a model
 * whose gateway entry has no ceiling ⇒ `undefined` ⇒ the engine leaves `max_tokens`
 * unclamped (graceful, no behaviour change). The `/model/info` map is fetched at most
 * once per process (cached on first success; failures are not cached, so the next turn
 * retries).
 */

/** Resolve a model id to its hard output ceiling (`max_output_tokens`), or `undefined`. */
export type ModelCeilingResolver = (model: string) => Promise<number | undefined>;

/** The subset of `fetch` this module needs — injectable so the resolver is unit-testable offline. */
export type FetchLike = (url: string, init?: { headers?: Record<string, string> }) => Promise<{ ok: boolean; json: () => Promise<unknown> }>;

/**
 * Map a gateway `/model/info` payload
 * (`{ data: [{ model_name, model_info: { max_output_tokens, … } }] }`) to a
 * `model_name → max_output_tokens` map. The TS analog of the Rust `map_model_info`,
 * narrowed to the one field the clamp needs. Entries without a `model_name`, without a
 * positive integer `max_output_tokens`, are skipped. Pure + network-free so it's
 * unit-testable on a sample payload.
 */
export function extractModelCeilings(payload: unknown): Map<string, number> {
    const out = new Map<string, number>();
    const data = (payload as { data?: unknown })?.data;
    if (!Array.isArray(data)) return out;
    for (const entry of data) {
        if (typeof entry !== 'object' || entry === null) continue;
        const name = (entry as { model_name?: unknown }).model_name;
        if (typeof name !== 'string' || name.length === 0) continue;
        const info = (entry as { model_info?: unknown }).model_info;
        const max = typeof info === 'object' && info !== null ? (info as { max_output_tokens?: unknown }).max_output_tokens : undefined;
        if (typeof max === 'number' && Number.isInteger(max) && max > 0) {
            out.set(name, max);
        }
    }
    return out;
}

/**
 * Build a {@link ModelCeilingResolver} backed by the gateway's `/model/info`.
 *
 * `gatewayUrl` is the OpenAI-compatible base url (e.g. `https://llm.smoo.ai/v1`); the
 * model-info endpoint is `{gatewayUrl}/model/info`. `gatewayKey`, when present, is
 * sent as a bearer token (the same creds the turns use). The whole model map is
 * fetched at most once per process on the first lookup and cached; a lost race just
 * recomputes the same stable map. Any error ⇒ every lookup returns `undefined` (and
 * the failure is not cached, so a later turn retries).
 */
export function createGatewayModelCeilingResolver(gatewayUrl: string, gatewayKey?: string, fetchImpl: FetchLike = fetch as unknown as FetchLike): ModelCeilingResolver {
    const url = `${gatewayUrl.replace(/\/+$/, '')}/model/info`;
    let cached: Map<string, number> | undefined;
    let inflight: Promise<Map<string, number>> | undefined;

    const load = async (): Promise<Map<string, number>> => {
        if (cached) return cached;
        // Single-flight: concurrent first turns share one fetch rather than stampeding
        // the gateway. On failure we return an empty map WITHOUT caching, so the next
        // turn retries (matches the Rust best-effort-not-cached-on-error behaviour).
        if (!inflight) {
            inflight = (async () => {
                try {
                    const res = await fetchImpl(url, gatewayKey ? { headers: { authorization: `Bearer ${gatewayKey}` } } : undefined);
                    if (!res.ok) throw new Error(`gateway /model/info returned non-ok`);
                    const map = extractModelCeilings(await res.json());
                    cached = map;
                    return map;
                } finally {
                    inflight = undefined;
                }
            })();
        }
        try {
            return await inflight;
        } catch {
            return new Map();
        }
    };

    return async (model: string): Promise<number | undefined> => {
        const ceiling = (await load()).get(model);
        return ceiling !== undefined && ceiling > 0 ? ceiling : undefined;
    };
}
