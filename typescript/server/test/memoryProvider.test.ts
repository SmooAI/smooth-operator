/**
 * Durable auto-recall (Rust PR #330 — the `StorageAdapter::memory_for_access` seam, tested in
 * `rust/smooth-operator-server/tests/injection_seams.rs`) — the TS server's parity.
 *
 * The engine already knew how to recall; what was missing on this server was the host's way to say
 * WHICH store, so every turn ran without auto-recall no matter what the deployment had. Tests are
 * named after their Rust counterparts so a parity gap stays visible.
 *
 * The recall block's header text is deliberately NOT asserted: the five cores currently inject three
 * different strings for it (th-ffaeae). The assertion is on the recalled CONTENT reaching the model,
 * which is the behavior the seam exists for.
 */
import { InMemoryMemory, MockLlmProvider } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import type { AccessContext } from '../src/auth.js';
import { StaticMemoryProvider, type MemoryProvider } from '../src/memory.js';
import { serve, type RunningServer } from '../src/server.js';
import { TestClient } from './wsClient.js';

const RECALLED = 'always add shows to the smoo-hub watchlist';

/** Everything the model was sent this turn, flattened — the surface a recalled memory must show up in. */
function allContentSeen(chat: MockLlmProvider): string {
    return JSON.stringify(chat.calls.map((c) => c.messages));
}

function memoryWithEntry(): InMemoryMemory {
    const memory = new InMemoryMemory();
    memory.remember(RECALLED);
    return memory;
}

describe('durable auto-recall (over the wire)', () => {
    let server: RunningServer | undefined;
    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    async function runTurn(memoryProvider: MemoryProvider | undefined, message: string): Promise<MockLlmProvider> {
        const chat = new MockLlmProvider().pushText('ok');
        server = await serve({ chatClient: chat, memoryProvider });
        const client = await TestClient.connect(server.url);
        client.sendAction({ action: 'create_conversation_session', requestId: 'cs', agentId: 'agent' });
        const sessionId = ((await client.receive()).data as Record<string, unknown>).sessionId as string;

        client.sendAction({ action: 'send_message', requestId: 'r2', sessionId, message });
        await client.receiveUntil('eventual_response');
        await client.close();
        return chat;
    }

    // ── rust: no_memory_means_no_recall_injection ────────────────────────────

    it('injects no recall when no provider is installed', async () => {
        // Guards against the seam injecting when absent — an unopted deployment's turn must be
        // byte-for-byte what it was before.
        const chat = await runTurn(undefined, 'add shows to my watchlist');
        expect(allContentSeen(chat)).not.toContain(RECALLED);
    });

    it('injects no recall when the provider declines this caller', async () => {
        // A provider returning undefined is the same as no provider — the seam must not fabricate a
        // store just because one was installed.
        const chat = await runTurn(new StaticMemoryProvider(undefined), 'add shows to my watchlist');
        expect(allContentSeen(chat)).not.toContain(RECALLED);
    });

    // ── rust: attached_memory_is_auto_recalled_into_the_turn ─────────────────

    it('auto-recalls an attached memory into the turn', async () => {
        // The message shares "add", "shows", "watchlist" with the stored entry, so the engine's
        // word-overlap recall surfaces it.
        const chat = await runTurn(new StaticMemoryProvider(memoryWithEntry()), 'add shows to my watchlist');
        expect(allContentSeen(chat)).toContain(RECALLED);
    });

    it('recalls nothing for an unrelated message', async () => {
        // Relevance-gated by the engine, not a blanket dump of every stored memory into every turn.
        // The message shares NO token with the entry — the bundled lexical scorer counts raw token
        // overlap with no stopword filter, so a single shared "the" would be enough to score a hit.
        const chat = await runTurn(new StaticMemoryProvider(memoryWithEntry()), 'explain quantum entanglement');
        expect(allContentSeen(chat)).not.toContain(RECALLED);
    });

    it('hands the provider the caller access, so a multi-tenant host can scope by requester', async () => {
        const seen: AccessContext[] = [];
        await runTurn(
            {
                memoryForAccess(access: AccessContext) {
                    seen.push(access);
                    return undefined;
                },
            },
            'hello',
        );
        expect(seen).toHaveLength(1);
    });
});
