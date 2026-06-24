/**
 * Graceful drain: a SIGTERM-equivalent (firing the shared cancel) mid-connection
 * lets an in-flight turn finish, then the read loop exits and the backplane detach
 * runs (detach-after-loop). The TS parity of the spec's drain requirement applied
 * across the Rust/C# servers.
 *
 * We build the server directly so the test can fire `drain.abort()` at a precise
 * moment and inspect the in-memory backplane to confirm detach ran.
 */
import type { ChatChunk, ChatClientLike } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { InMemoryBackplane } from '../src/backplane.js';
import { buildServer } from '../src/server.js';
import { TestClient } from './wsClient.js';

/**
 * A streaming chat client whose model stream BLOCKS on a gate until the test
 * releases it — so a turn is provably "in flight" when the test fires the cancel.
 * Implements the engine's `createStream` seam (what `runStream` drives).
 */
class GatedStreamClient implements ChatClientLike {
    started = false;
    private release!: () => void;
    private readonly gate = new Promise<void>((resolve) => {
        this.release = resolve;
    });
    private readonly text: string;

    constructor(text: string) {
        this.text = text;
    }

    /** Resolves once the model stream has begun (the turn is in flight). */
    waitUntilStarted(): Promise<void> {
        return new Promise<void>((resolve) => {
            const poll = () => (this.started ? resolve() : setTimeout(poll, 5));
            poll();
        });
    }

    /** Let the blocked stream complete. */
    finish(): void {
        this.release();
    }

    readonly chat = {
        completions: {
            // Non-streaming path is unused by runStream, but the seam requires it.
            create: async () => ({ choices: [{ message: { content: this.text, tool_calls: null } }], usage: null }),
            createStream: (_body: Record<string, unknown>): AsyncIterable<ChatChunk> => {
                const self = this;
                return {
                    async *[Symbol.asyncIterator](): AsyncGenerator<ChatChunk> {
                        self.started = true;
                        await self.gate; // block until the test releases us
                        yield { choices: [{ delta: { content: self.text } }], usage: { prompt_tokens: 1, completion_tokens: 1 } };
                    },
                };
            },
        },
    };
}

describe('graceful drain', () => {
    let close: (() => Promise<void>) | undefined;

    afterEach(async () => {
        await close?.();
        close = undefined;
    });

    it('an in-flight turn finishes after cancel fires, then the loop exits and detach runs', async () => {
        const backplane = new InMemoryBackplane();
        const chat = new GatedStreamClient('Drained answer.');
        const { http, wss, drain } = buildServer({ chatClient: chat, backplane });

        await new Promise<void>((resolve) => http.listen(0, '127.0.0.1', () => resolve()));
        const address = http.address();
        const port = typeof address === 'object' && address ? address.port : 0;
        close = async () => {
            drain.abort();
            await new Promise<void>((r) => wss.close(() => r()));
            await new Promise<void>((r) => http.close(() => r()));
        };

        const client = await TestClient.connect(`ws://127.0.0.1:${port}/ws`);
        client.sendAction({ action: 'create_conversation_session', requestId: 'cs' });
        const sessionId = ((await client.receive()).data as Record<string, unknown>).sessionId as string;

        // The connection is attached to the backplane while the read loop runs.
        expect(backplane.size).toBe(1);

        client.sendAction({ action: 'send_message', requestId: 'sm', sessionId, message: 'hi' });
        const ack = await client.receive();
        expect(ack.status).toBe(202);

        // Wait until the turn is provably in flight (the model stream has started).
        await chat.waitUntilStarted();

        // Fire the SIGTERM-equivalent cancel WHILE the turn is in flight.
        drain.abort();

        // The in-flight turn still completes — it was awaited inside the frame branch,
        // so cancel doesn't sever it. Release the gated stream and expect the terminal.
        chat.finish();
        const { terminal } = await client.receiveUntil('eventual_response');
        expect(terminal.type).toBe('eventual_response');

        // detach-after-loop: once the read loop exits, the backplane is empty.
        await waitFor(() => backplane.size === 0, 3000);
        expect(backplane.size).toBe(0);

        await client.close();
    });

    it('cancel fired before any frame stops the loop and still detaches', async () => {
        const backplane = new InMemoryBackplane();
        const chat = new GatedStreamClient('unused');
        const { http, wss, drain } = buildServer({ chatClient: chat, backplane });

        await new Promise<void>((resolve) => http.listen(0, '127.0.0.1', () => resolve()));
        const address = http.address();
        const port = typeof address === 'object' && address ? address.port : 0;
        close = async () => {
            drain.abort();
            await new Promise<void>((r) => wss.close(() => r()));
            await new Promise<void>((r) => http.close(() => r()));
        };

        const client = await TestClient.connect(`ws://127.0.0.1:${port}/ws`);
        await waitFor(() => backplane.size === 1, 2000);

        // Idle connection (no in-flight turn): cancel drains it immediately.
        drain.abort();
        await waitFor(() => backplane.size === 0, 3000);
        expect(backplane.size).toBe(0);

        await client.close();
    });
});

async function waitFor(predicate: () => boolean, timeoutMs: number): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
        if (predicate()) return;
        await new Promise((r) => setTimeout(r, 10));
    }
    throw new Error('condition not met within timeout');
}
