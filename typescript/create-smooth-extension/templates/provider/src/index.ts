/**
 * __NAME__ — a SEP LLM-provider extension.
 *
 * Registers a provider the host reaches over `provider/complete`; this extension
 * does the real completion call, streams chunks back as `provider/delta`, and
 * (optionally) mediates OAuth. Out of the box it returns a canned echo response
 * so it builds and tests green with no network — replace the marked section with
 * your real HTTP call (see the `corporate-proxy` demo for a full OpenAI-compatible
 * proxy). Run it with `node dist/index.js`.
 */
import { defineExtension, defineProvider, type ProviderCompleteResult, type ProviderStreamEvent } from '@smooai/smooth-extension-sdk';

export const extension = defineExtension((smooth) => {
    smooth.name = '__NAME__';
    smooth.version = '0.1.0';

    // Live config — `apiKey` is mutable so an OAuth login can populate it.
    const state = {
        baseUrl: process.env.PROVIDER_BASE_URL ?? '',
        apiKey: process.env.PROVIDER_API_KEY ?? '',
    };

    smooth.registerProvider(
        defineProvider({
            name: '__NAME__',
            baseUrl: state.baseUrl || undefined,
            apiKeyEnv: 'PROVIDER_API_KEY',
            models: [{ id: '__NAME__-1', display_name: '__NAME__ model' }],

            async complete(req, ctx): Promise<ProviderCompleteResult> {
                const lastUser = [...req.messages].reverse().find((m) => m.role === 'user');
                const reply = `echo: ${String(lastUser?.content ?? '')}`;

                // === Replace this block with your real completion call ==========
                // const res = await fetch(`${state.baseUrl}/chat/completions`, {
                //     method: 'POST',
                //     headers: { authorization: `Bearer ${state.apiKey}`, 'content-type': 'application/json' },
                //     body: JSON.stringify({ model: req.model, messages: req.messages, stream: req.stream }),
                //     signal: ctx.signal,
                // });
                // ...stream res.body, emitting ctx.delta(...) per chunk...
                // ================================================================

                if (req.stream) {
                    for (const content of reply.match(/.{1,4}/g) ?? []) {
                        ctx.delta({ type: 'Delta', content } satisfies ProviderStreamEvent);
                    }
                }
                return { content: reply, finish_reason: 'stop', resolved_model: req.model };
            },

            // Optional OAuth — delete if the provider uses a static API key.
            async oauthLogin() {
                return { api_key: state.apiKey || 'replace-me', refresh_token: 'replace-me' };
            },
            async oauthRefresh(refreshToken) {
                return { api_key: state.apiKey || 'replace-me', refresh_token: refreshToken };
            },
        }),
    );
});

// When run directly (not imported by a test), serve over stdio.
if (import.meta.url === `file://${process.argv[1]}`) {
    extension.serve();
}
