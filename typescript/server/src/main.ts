#!/usr/bin/env node
/**
 * The binary entrypoint — boot a server from the environment.
 *
 * The TS analog of the Rust server's `main.rs` + the local flavor's `serve_local`.
 * Defaults to the LOCAL flavor (in-memory everything, loopback, auth off). The LLM
 * gateway is read from `SMOOAI_GATEWAY_URL` / `SMOOAI_GATEWAY_KEY`; with no key,
 * `send_message` returns a clean protocol `error` exactly as the keyless test path
 * does (the engine has no client to call).
 *
 * Env:
 *   SMOOTH_OPERATOR_HOST   bind host   (default 127.0.0.1)
 *   SMOOTH_OPERATOR_PORT   bind port   (default 8787)
 *   SMOOAI_GATEWAY_URL     OpenAI-compatible base URL (enables live turns with a key)
 *   SMOOAI_GATEWAY_KEY     gateway API key
 *   SMOOAI_MODEL           model id    (default claude-haiku-4-5)
 */
import type { ChatClientLike } from '@smooai/smooth-operator-core';

import { createGatewayModelCeilingResolver, type ModelCeilingResolver } from './modelCeiling.js';
import { serveLocal } from './server.js';

/** The model id turns run against — SMOOAI_MODEL, else the engine's default. */
function resolveModel(): string {
    return process.env.SMOOAI_MODEL ?? 'claude-haiku-4-5';
}

/**
 * A per-model output-ceiling resolver backed by the gateway's `/model/info`, so each
 * turn clamps `max_tokens` to what the model can physically emit (EPIC th-1cc9fa). Only
 * built when a gateway url+key are configured; otherwise `undefined` ⇒ turns run
 * unclamped (behaviour unchanged on the keyless local path).
 */
function buildModelCeiling(): ModelCeilingResolver | undefined {
    const url = process.env.SMOOAI_GATEWAY_URL;
    const key = process.env.SMOOAI_GATEWAY_KEY;
    if (!url || !key) return undefined;
    return createGatewayModelCeilingResolver(url, key);
}

/**
 * A keyless client: every model call rejects, so `send_message` surfaces a clean
 * protocol error (the dispatcher's catch → INTERNAL_ERROR) instead of hanging. The
 * parity of the Rust "no gateway key" path. Replace by pointing the engine at a real
 * OpenAI-compatible client when a gateway key is configured.
 */
function keylessClient(): ChatClientLike {
    const fail = (): never => {
        throw new Error('No LLM gateway configured (set SMOOAI_GATEWAY_URL + SMOOAI_GATEWAY_KEY)');
    };
    return {
        chat: {
            completions: {
                create: () => Promise.reject(fail()),
            },
        },
    } as ChatClientLike;
}

async function buildChatClient(): Promise<ChatClientLike> {
    const url = process.env.SMOOAI_GATEWAY_URL;
    const key = process.env.SMOOAI_GATEWAY_KEY;
    if (!url || !key) {
        return keylessClient();
    }
    // A gateway key is present: wire the real OpenAI-compatible client. `openai` is
    // an optional, lazily-imported dependency so the keyless local flavor needs no
    // extra install. (Importing it only on this path keeps the MVP dependency-light.)
    try {
        // Indirected through a variable so the bundler/typechecker treats `openai`
        // as an OPTIONAL runtime dependency (it isn't in this package's deps — the
        // keyless local flavor needs no LLM SDK). Production installs it alongside.
        const openaiModule = 'openai';
        const mod = (await import(openaiModule)) as { default: new (opts: { apiKey: string; baseURL: string }) => ChatClientLike };
        // Pin the resolved model into the env so the turn runner and the ceiling lookup
        // agree on which model is in play (the request model and its /model/info ceiling
        // must be the same model).
        process.env.SMOOAI_MODEL = resolveModel();
        const openai = new mod.default({ apiKey: key, baseURL: url });
        // The engine's `runStream` needs `chat.completions.createStream`; the raw SDK
        // only exposes `create`. Adapt it: streaming is `create({ ...body, stream: true })`,
        // whose async-iterable of chunks already matches the engine's `ChatChunk` shape.
        // Without this the server boots but every turn throws "requires a streaming-capable
        // client" (the server always uses runStream). ponytail: thin wrapper, no new dep.
        const completions = openai.chat.completions;
        completions.createStream = async function* (body: Record<string, unknown>) {
            // openai's `create({stream:true})` resolves to a Stream; the engine wants a
            // synchronous AsyncIterable, so await it here and re-yield its chunks.
            const stream = (await completions.create({ ...body, stream: true })) as unknown as AsyncIterable<import('@smooai/smooth-operator-core').ChatChunk>;
            yield* stream;
        };
        return openai;
    } catch {
        // The `openai` package isn't installed — fall back to the keyless client so
        // the server still boots (turns error cleanly).
        return keylessClient();
    }
}

async function main(): Promise<void> {
    const host = process.env.SMOOTH_OPERATOR_HOST ?? '127.0.0.1';
    const port = Number(process.env.SMOOTH_OPERATOR_PORT ?? '8787');
    const chatClient = await buildChatClient();

    const server = await serveLocal({ chatClient, host, port, model: resolveModel(), modelCeiling: buildModelCeiling() });
    // eslint-disable-next-line no-console
    console.log(`smooth-operator-server (TypeScript, local flavor) listening on ${server.url}`);
    // serveLocal already wires SIGTERM/SIGINT → graceful drain + close.
}

main().catch((err) => {
    // eslint-disable-next-line no-console
    console.error('smooth-operator-server failed to start:', err);
    process.exit(1);
});
