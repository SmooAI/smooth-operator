/**
 * `corporate-proxy` — the Phase 7 demo extension (pi's custom-provider port).
 *
 * Registers an LLM provider that proxies an OpenAI-compatible endpoint (a
 * corporate LLM gateway). The host reaches it over `provider/complete`; this
 * extension does the real HTTP call, streams the upstream SSE back as
 * `provider/delta` chunks, and mediates OAuth (`provider/oauth_login` /
 * `oauth_refresh`) — driving the login prompt through the `ui/*` surface.
 *
 * Config comes from the environment (a real deployment) or the factory arg (so
 * the integration test can point `baseUrl` at a mock upstream):
 *   - `CORPORATE_PROXY_BASE_URL` — OpenAI-compatible base, e.g. https://llm.corp/v1
 *   - `CORPORATE_PROXY_API_KEY`  — bearer key (or obtained via OAuth)
 *
 * Run it as a real SEP subprocess:  `tsx examples/corporate-proxy.ts`
 */
import { defineExtension, defineProvider, type ProviderCompleteResult, type ProviderStreamEvent } from '../src/index.js';

export interface CorporateProxyConfig {
    /** OpenAI-compatible base URL. Default: `CORPORATE_PROXY_BASE_URL`. */
    baseUrl?: string;
    /** Bearer API key. Default: `CORPORATE_PROXY_API_KEY`. Mutated by OAuth. */
    apiKey?: string;
    /** Injectable fetch (tests point this at a mock upstream). Default: global `fetch`. */
    fetchImpl?: typeof fetch;
}

/** Map the host's serialized messages onto OpenAI chat messages (role + content,
 *  plus tool-call plumbing when present). The host is the source of truth for the
 *  shape; we forward the fields an OpenAI-compatible endpoint expects. */
function toOpenAiMessages(messages: Record<string, unknown>[]): Record<string, unknown>[] {
    return messages.map((m) => {
        const out: Record<string, unknown> = { role: m.role, content: m.content ?? '' };
        if (m.tool_calls) out.tool_calls = m.tool_calls;
        if (m.tool_call_id) out.tool_call_id = m.tool_call_id;
        if (m.tool_name) out.name = m.tool_name;
        return out;
    });
}

/** Pull the reasoning `effort` from a thinking level, for OpenAI o-series style
 *  `reasoning_effort`. Unknown tokens pass through verbatim. */
function reasoningEffort(thinking: string | undefined): string | undefined {
    if (!thinking || thinking === 'off') return undefined;
    return thinking;
}

/** Parse one SSE `data:` payload into a stream event + accumulation, OpenAI SSE
 *  chunk shape. Returns null for `[DONE]` and unparseable/empty chunks. */
function parseSseChunk(data: string): { content: string; finishReason?: string; model?: string; usage?: Record<string, number> } | null {
    if (data === '[DONE]') return null;
    let chunk: {
        choices?: { delta?: { content?: string }; finish_reason?: string }[];
        model?: string;
        usage?: Record<string, number>;
    };
    try {
        chunk = JSON.parse(data);
    } catch {
        return null;
    }
    const choice = chunk.choices?.[0];
    return {
        content: choice?.delta?.content ?? '',
        ...(choice?.finish_reason ? { finishReason: choice.finish_reason } : {}),
        ...(chunk.model ? { model: chunk.model } : {}),
        ...(chunk.usage ? { usage: chunk.usage } : {}),
    };
}

export const createCorporateProxy = (config: CorporateProxyConfig = {}) =>
    defineExtension((smooth) => {
        smooth.name = 'corporate-proxy';
        smooth.version = '0.1.0';

        // Live config: `apiKey` is mutable so an OAuth login can populate it.
        const state = {
            baseUrl: config.baseUrl ?? process.env.CORPORATE_PROXY_BASE_URL ?? '',
            apiKey: config.apiKey ?? process.env.CORPORATE_PROXY_API_KEY ?? '',
        };
        const doFetch = config.fetchImpl ?? fetch;

        smooth.registerProvider(
            defineProvider({
                name: 'corporate-proxy',
                baseUrl: state.baseUrl,
                apiKeyEnv: 'CORPORATE_PROXY_API_KEY',
                models: [
                    { id: 'corp-gpt-4o', display_name: 'Corporate GPT-4o' },
                    { id: 'corp-fast', display_name: 'Corporate Fast' },
                ],

                async complete(req, ctx) {
                    const body: Record<string, unknown> = {
                        model: req.model,
                        messages: toOpenAiMessages(req.messages),
                        stream: req.stream,
                    };
                    if (req.tools.length) body.tools = req.tools;
                    if (req.responseFormat) body.response_format = req.responseFormat;
                    const effort = reasoningEffort(req.thinking);
                    if (effort) body.reasoning_effort = effort;

                    const res = await doFetch(`${state.baseUrl}/chat/completions`, {
                        method: 'POST',
                        headers: {
                            'content-type': 'application/json',
                            ...(state.apiKey ? { authorization: `Bearer ${state.apiKey}` } : {}),
                        },
                        body: JSON.stringify(body),
                        signal: ctx.signal,
                    });
                    if (!res.ok) {
                        const text = await res.text().catch(() => res.statusText);
                        throw new Error(`corporate-proxy upstream ${res.status}: ${text}`);
                    }

                    if (!req.stream || !res.body) {
                        const json = (await res.json()) as {
                            choices?: { message?: { content?: string; tool_calls?: { id: string; function?: { name: string; arguments: string } }[] }; finish_reason?: string }[];
                            model?: string;
                            usage?: Record<string, number>;
                        };
                        const choice = json.choices?.[0];
                        const result: ProviderCompleteResult = {
                            content: choice?.message?.content ?? '',
                            finish_reason: choice?.finish_reason ?? 'stop',
                            ...(json.model ? { resolved_model: json.model } : {}),
                            ...(json.usage ? { usage: json.usage } : {}),
                        };
                        const calls = choice?.message?.tool_calls;
                        if (calls?.length) {
                            result.tool_calls = calls.map((c) => ({
                                id: c.id,
                                name: c.function?.name ?? '',
                                arguments: c.function?.arguments ? safeJson(c.function.arguments) : {},
                            }));
                        }
                        return result;
                    }

                    // Streaming: parse the upstream SSE, forward each content delta
                    // as a `provider/delta`, and assemble the final result.
                    let content = '';
                    let finishReason = 'stop';
                    let model: string | undefined;
                    let usage: Record<string, number> | undefined;
                    const reader = res.body.getReader();
                    const decoder = new TextDecoder();
                    let buffer = '';
                    for (;;) {
                        const { value, done } = await reader.read();
                        if (done) break;
                        buffer += decoder.decode(value, { stream: true });
                        // SSE events are separated by a blank line; each `data:` line is a chunk.
                        let nl: number;
                        while ((nl = buffer.indexOf('\n')) !== -1) {
                            const line = buffer.slice(0, nl).trim();
                            buffer = buffer.slice(nl + 1);
                            if (!line.startsWith('data:')) continue;
                            const parsed = parseSseChunk(line.slice(5).trim());
                            if (!parsed) continue;
                            if (parsed.content) {
                                content += parsed.content;
                                const event: ProviderStreamEvent = { type: 'Delta', content: parsed.content };
                                ctx.delta(event);
                            }
                            if (parsed.finishReason) finishReason = parsed.finishReason;
                            if (parsed.model) model = parsed.model;
                            if (parsed.usage) usage = parsed.usage;
                        }
                    }
                    return {
                        content,
                        finish_reason: finishReason,
                        ...(model ? { resolved_model: model } : {}),
                        ...(usage ? { usage } : {}),
                    };
                },

                async oauthLogin(ctx) {
                    // Real deployments would open a browser to the authorize URL; the
                    // demo surfaces it and asks the user to paste the resulting code.
                    const authorizeUrl = `${state.baseUrl}/oauth/authorize`;
                    if (ctx.hasUI('notify')) await ctx.ui.notify(`Open ${authorizeUrl} to authorize corporate-proxy`);
                    let code = 'demo-code';
                    if (ctx.hasUI('input')) {
                        const answer = await ctx.ui.input('Paste the authorization code');
                        if (answer.cancelled) throw new Error('corporate-proxy login cancelled');
                        code = answer.text ?? code;
                    }
                    const creds = await exchange({ grant_type: 'authorization_code', code });
                    state.apiKey = creds.api_key ?? state.apiKey;
                    return creds;
                },

                async oauthRefresh(refreshToken) {
                    const creds = await exchange({ grant_type: 'refresh_token', refresh_token: refreshToken });
                    state.apiKey = creds.api_key ?? state.apiKey;
                    return creds;
                },
            }),
        );

        /** POST the token endpoint and shape the response into ProviderCredentials. */
        async function exchange(form: Record<string, string>) {
            const res = await doFetch(`${state.baseUrl}/oauth/token`, {
                method: 'POST',
                headers: { 'content-type': 'application/json' },
                body: JSON.stringify(form),
            });
            if (!res.ok) throw new Error(`corporate-proxy token endpoint ${res.status}`);
            const json = (await res.json()) as { access_token?: string; refresh_token?: string; expires_in?: number };
            return {
                api_key: json.access_token,
                access_token: json.access_token,
                ...(json.refresh_token ? { refresh_token: json.refresh_token } : {}),
                ...(json.expires_in ? { expires_at: Math.floor(Date.now() / 1000) + json.expires_in } : {}),
            };
        }
    });

function safeJson(s: string): unknown {
    try {
        return JSON.parse(s);
    } catch {
        return {};
    }
}

// Served singleton: one provider, config from the environment.
if (import.meta.url === `file://${process.argv[1]}`) {
    createCorporateProxy().serve();
}
