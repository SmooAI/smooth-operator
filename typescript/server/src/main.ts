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
 *   SMOOAI_MODEL           model id    (default gpt-4o-mini)
 */
import type { ChatClientLike } from '@smooai/smooth-operator-core';

import { serveLocal } from './server.js';

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
        const model = process.env.SMOOAI_MODEL ?? 'gpt-4o-mini';
        process.env.SMOOAI_MODEL = model;
        return new mod.default({ apiKey: key, baseURL: url });
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

    const server = await serveLocal({ chatClient, host, port });
    // eslint-disable-next-line no-console
    console.log(`smooth-operator-server (TypeScript, local flavor) listening on ${server.url}`);
    // serveLocal already wires SIGTERM/SIGINT → graceful drain + close.
}

main().catch((err) => {
    // eslint-disable-next-line no-console
    console.error('smooth-operator-server failed to start:', err);
    process.exit(1);
});
