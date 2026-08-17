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
 * Env (canonical names, shared with the Rust/Go/Python/.NET hosts — this host's
 * pre-parity `SMOOTH_OPERATOR_HOST` / `SMOOTH_OPERATOR_PORT` / `SMOOAI_MODEL` still
 * work as aliases; see `env.ts` for the full table):
 *   SMOOTH_AGENT_BIND      bind host   (default 127.0.0.1)
 *   SMOOTH_AGENT_PORT      bind port   (default 8787)
 *   SMOOTH_AGENT_MODEL     model id    (default claude-haiku-4-5)
 *   SMOOAI_GATEWAY_URL     OpenAI-compatible base URL (enables live turns with a key)
 *   SMOOAI_GATEWAY_KEY     gateway API key
 *   SMOOTH_WORKSPACE       root the coding tools are confined to (default: cwd)
 *   SMOOTH_NO_TOOLS        set to "1" to serve a chat-only agent (no coding tools)
 *   SMOOTH_AGENT_STORAGE   memory (default) | postgres — durable sessions + admin stores
 *   SMOOTH_AGENT_DATABASE_URL  Postgres DSN for SMOOTH_AGENT_STORAGE=postgres (falls
 *                          back to DATABASE_URL, but only once postgres is asked for)
 */
import { pathToFileURL } from 'node:url';

import { createGatewayClient } from '@smooai/smooth-operator-core';
import type { ChatClientLike, Tool } from '@smooai/smooth-operator-core';

import { codingTools } from './codingTools.js';
import { resolveBind, resolveModel } from './env.js';
import { resolveStorage } from './postgresStore.js';
import { createGatewayModelCeilingResolver, type ModelCeilingResolver } from './modelCeiling.js';
import { serveLocal } from './server.js';

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

// Exported for the wiring test: injecting a header-DROPPING client is exactly the
// bug this file fixes, and only a test that calls this can catch a regression.
export async function buildChatClient(): Promise<ChatClientLike> {
    const url = process.env.SMOOAI_GATEWAY_URL;
    const key = process.env.SMOOAI_GATEWAY_KEY;
    if (!url || !key) {
        return keylessClient();
    }
    // Pin the resolved model into the env so the turn runner and the ceiling lookup
    // agree on which model is in play (the request model and its /model/info ceiling
    // must be the same model). Pinned under the CANONICAL name, which `resolveModel`
    // reads first — pinning the alias would be overridden by a set canonical name.
    process.env.SMOOTH_AGENT_MODEL = resolveModel();
    // Core's own client, not the raw `openai` SDK. The gateway reports per-request cost
    // ONLY in a response header and the SDK's parsed response drops headers, so core's
    // cost-header parser had nothing to read and every turn's costUsd came back 0.
    // createGatewayClient keeps the response (`.withResponse()`) and surfaces the cost —
    // and it brings a real `createStream`, so the hand-rolled adapter this replaced,
    // which had no way to carry a cost at all, is gone. Same reason the Go host injects
    // core.NewGatewayClient. `openai` arrives transitively through core, so the optional
    // lazy-import dance (and its swallow-everything catch) is gone with it.
    return createGatewayClient({ baseURL: url, apiKey: key });
}

/**
 * Give the agent a workspace-confined coding toolset (read/write/edit/list/grep/bash) so
 * it can actually edit files — without `tools` the local agent is chat-only and replies
 * "I don't have file editing tools" (th-82ad57). Confined to `SMOOTH_WORKSPACE` (default:
 * the process cwd, which is what the bench launches the server in). Mirrors the Go serve
 * binary's env contract.
 */
function buildTools(): Tool[] | undefined {
    if (process.env.SMOOTH_NO_TOOLS === '1') return undefined;
    const workspace = process.env.SMOOTH_WORKSPACE || process.cwd();
    // eslint-disable-next-line no-console
    console.log(`coding tools enabled, confined to workspace: ${workspace}`);
    return codingTools(workspace);
}

async function main(): Promise<void> {
    const { host, port } = resolveBind();
    const chatClient = await buildChatClient();
    // Unset/memory → undefined, and the local flavor stays fully in-memory. A
    // misconfigured durable backend throws out of main() rather than silently falling
    // back to memory: losing durability quietly is the failure worth shouting about.
    const storage = await resolveStorage();
    if (storage) {
        // eslint-disable-next-line no-console
        console.log(`durable storage enabled: SMOOTH_AGENT_STORAGE=${process.env.SMOOTH_AGENT_STORAGE}`);
    }

    const server = await serveLocal({ chatClient, host, port, model: resolveModel(), modelCeiling: buildModelCeiling(), tools: buildTools(), storage });
    // eslint-disable-next-line no-console
    console.log(`smooth-operator-server (TypeScript, local flavor) listening on ${server.url}`);
    // serveLocal already wires SIGTERM/SIGINT → graceful drain + close.
}

// Boot only when run as the binary, not when imported. Without this guard, merely
// importing anything from this module starts a server on the default port — which
// the wiring test below does, and which would make it a port-collision flake.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
    main().catch((err) => {
        // eslint-disable-next-line no-console
        console.error('smooth-operator-server failed to start:', err);
        process.exit(1);
    });
}
