/**
 * Turn round-trip: a real WS client connects → create_conversation_session →
 * send_message (engine driven by MockLlmProvider) → receives the 202 ack, at least
 * one stream_token, and a terminal eventual_response carrying the expected text.
 *
 * The TS parity of the C# `WebSocketProtocolIntegrationTests`
 * (FullConversation_OverRealWebSocket + ToolCall + ACL/citations).
 */
import { InMemoryKnowledge, MockLlmProvider } from '@smooai/smooth-operator-core';
import type { Knowledge } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import type { AccessKnowledge } from '../src/frameDispatcher.js';
import { serve, type RunningServer } from '../src/server.js';
import { TestClient } from './wsClient.js';

describe('turn round-trip over a real WebSocket', () => {
    let server: RunningServer | undefined;

    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('ping → create session → send_message → tokens → eventual_response', async () => {
        server = await serve({ chatClient: new MockLlmProvider().pushText('Your return window is 17 days.') });
        const client = await TestClient.connect(server.url);

        // create_conversation_session → descriptor.
        const agentId = '11111111-1111-1111-1111-111111111111';
        client.sendAction({ action: 'create_conversation_session', requestId: 'cs-1', agentId, userName: 'Test' });
        const created = await client.receive();
        expect(created.type).toBe('immediate_response');
        expect(created.status).toBe(200);
        const data = created.data as Record<string, unknown>;
        const sessionId = data.sessionId as string;
        expect(sessionId).toMatch(/^[0-9a-f-]{36}$/);
        expect(data.agentId).toBe(agentId); // echoed back

        // send_message → 202 ack → stream_token(s) → eventual_response.
        client.sendAction({ action: 'send_message', requestId: 'sm-1', sessionId, message: 'How long can I return?' });
        const ack = await client.receive();
        expect(ack.type).toBe('immediate_response');
        expect(ack.status).toBe(202);

        const { terminal, seen } = await client.receiveUntil('eventual_response');
        expect(seen.some((f) => f.type === 'stream_token')).toBe(true);

        const inner = (terminal.data as Record<string, unknown>).data as Record<string, unknown>;
        const parts = (inner.response as Record<string, unknown>).responseParts as string[];
        expect(parts.some((p) => p.includes('17 days'))).toBe(true);

        await client.close();
    });

    it('emits a tool-call chunk and a tool-result chunk for a tool turn', async () => {
        // Iteration 1: the model requests a tool. No tool is registered, so the engine
        // returns an "unknown tool" result (still exercises the chunk lifecycle), feeds
        // it back, and iteration 2 answers.
        const chat = new MockLlmProvider()
            .pushToolCall('call-1', 'lookup_policy', JSON.stringify({ topic: 'returns' }))
            .pushText('Our return window is 30 days.');
        server = await serve({ chatClient: chat });
        const client = await TestClient.connect(server.url);

        client.sendAction({ action: 'create_conversation_session', requestId: 'cs' });
        const sessionId = ((await client.receive()).data as Record<string, unknown>).sessionId as string;

        client.sendAction({ action: 'send_message', requestId: 'sm', sessionId, message: 'how long to return?' });
        const { seen } = await client.receiveUntil('eventual_response');

        const chunks = seen.filter((f) => f.type === 'stream_chunk');
        const callChunk = chunks.find((c) => {
            const state = ((c.data as Record<string, unknown>).state as Record<string, unknown>).rawResponse as Record<string, unknown>;
            return (state?.toolCall as Record<string, unknown>)?.name === 'lookup_policy';
        });
        expect(callChunk, 'a tool-call chunk for lookup_policy').toBeDefined();

        const resultChunk = chunks.find((c) => {
            const state = ((c.data as Record<string, unknown>).state as Record<string, unknown>).rawResponse as Record<string, unknown>;
            return state?.toolResult !== undefined;
        });
        expect(resultChunk, 'a tool-result chunk').toBeDefined();
        expect(resultChunk!.node).toBe('lookup_policy');

        expect(seen.some((f) => f.type === 'stream_token')).toBe(true);
        await client.close();
    });

    it('grounds the turn with citations from scoped knowledge', async () => {
        const kb = new InMemoryKnowledge();
        kb.ingest('Refund policy: returns are accepted within 30 days for a full refund.', 'https://example.com/returns');
        kb.ingest('Shipping takes 5 to 7 business days.', 'shipping.md');

        const knowledge: AccessKnowledge = { forAccess: (): Knowledge => kb };
        server = await serve({ chatClient: new MockLlmProvider().pushText('Returns are accepted within 30 days.'), knowledge });
        const client = await TestClient.connect(server.url);

        client.sendAction({ action: 'create_conversation_session', requestId: 'cs' });
        const sessionId = ((await client.receive()).data as Record<string, unknown>).sessionId as string;

        client.sendAction({ action: 'send_message', requestId: 'sm', sessionId, message: 'what is the refund policy?' });
        const { terminal } = await client.receiveUntil('eventual_response');

        const inner = (terminal.data as Record<string, unknown>).data as Record<string, unknown>;
        const citations = inner.citations as Array<Record<string, unknown>>;
        expect(citations.length).toBeGreaterThan(0);
        // The http-sourced doc carries a url; the local .md doc omits it.
        const withUrl = citations.find((c) => c.url === 'https://example.com/returns');
        expect(withUrl).toBeDefined();
        const local = citations.find((c) => c.title === 'shipping.md');
        if (local) expect('url' in local).toBe(false);

        await client.close();
    });

    it('multi-turn history is replayed so the second turn sees the first', async () => {
        // Two scripted answers; the second model call must receive the prior user +
        // assistant turn as history (proving the session store + thread replay wired).
        const chat = new MockLlmProvider().pushText('First answer.').pushText('Second answer.');
        server = await serve({ chatClient: chat });
        const client = await TestClient.connect(server.url);

        client.sendAction({ action: 'create_conversation_session', requestId: 'cs' });
        const sessionId = ((await client.receive()).data as Record<string, unknown>).sessionId as string;

        client.sendAction({ action: 'send_message', requestId: 'sm-1', sessionId, message: 'first question' });
        await client.receiveUntil('eventual_response');

        client.sendAction({ action: 'send_message', requestId: 'sm-2', sessionId, message: 'second question' });
        await client.receiveUntil('eventual_response');

        // The mock recorded both calls; the second call's messages include the first
        // turn's user + assistant messages (history replay).
        expect(chat.callCount).toBe(2);
        const secondCallMessages = chat.calls[1]!.messages as Array<Record<string, unknown>>;
        const contents = secondCallMessages.map((m) => m.content);
        expect(contents).toContain('first question');
        expect(contents).toContain('First answer.');

        await client.close();
    });

    it('send_message to an unknown session errors without dropping the connection', async () => {
        server = await serve({ chatClient: new MockLlmProvider().pushText('unused') });
        const client = await TestClient.connect(server.url);

        client.sendAction({ action: 'send_message', requestId: 'sm', sessionId: 'does-not-exist', message: 'hi' });
        const error = await client.receive();
        expect(error.type).toBe('error');
        // Parity with the Python/Rust reference + the conformance corpus: the
        // descriptor lives both at the envelope level (`error`) and nested under
        // `data.error`, with code `SESSION_NOT_FOUND`.
        expect((error.error as Record<string, unknown>).code).toBe('SESSION_NOT_FOUND');
        expect(((error.data as Record<string, unknown>).error as Record<string, unknown>).code).toBe('SESSION_NOT_FOUND');

        client.sendAction({ action: 'ping', requestId: 'after' });
        expect((await client.receive()).type).toBe('pong');
        await client.close();
    });
});
