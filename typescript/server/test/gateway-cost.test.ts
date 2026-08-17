/**
 * The gateway's per-request cost reaches `eventual_response.usage.costUsd`.
 *
 * Cost is reported ONLY in a response header. The server used to inject the raw
 * `openai` SDK, whose parsed response drops headers — so core's cost-header parser
 * had nothing to read and every turn reported `costUsd: 0`. These drive a REAL
 * WebSocket client against a REAL local gateway (`node:http` speaking SSE), so they
 * fail if the server ever goes back to injecting a header-dropping client.
 *
 * Asserted at the PROTOCOL boundary, not inside the engine: the engine already had
 * working cost accounting: what was broken was the wiring, and only the frame the
 * client actually receives proves the wiring.
 */
import { createGatewayClient } from '@smooai/smooth-operator-core';
import { createServer, type Server } from 'node:http';
import { afterEach, describe, expect, it } from 'vitest';

import { buildChatClient } from '../src/main.js';
import { serve, type RunningServer } from '../src/server.js';
import { TestClient } from './wsClient.js';

let gateway: Server | undefined;
let server: RunningServer | undefined;

/** Start a local OpenAI-compatible gateway that streams one SSE reply. */
async function startGateway(headers: Record<string, string>): Promise<string> {
    gateway = createServer((req, res) => {
        req.resume();
        req.on('end', () => {
            for (const [name, value] of Object.entries(headers)) res.setHeader(name, value);
            res.setHeader('Content-Type', 'text/event-stream');
            res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: 'Seventeen days.' } }] })}\n\n`);
            res.write(`data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 10, completion_tokens: 5 } })}\n\n`);
            res.write('data: [DONE]\n\n');
            res.end();
        });
    });
    await new Promise<void>((resolve) => gateway!.listen(0, '127.0.0.1', resolve));
    const addr = gateway.address();
    return `http://127.0.0.1:${typeof addr === 'object' && addr ? addr.port : 0}/v1`;
}

/** Run one full turn and return its `eventual_response.usage`. */
async function turnUsage(headers: Record<string, string>): Promise<Record<string, number> | undefined> {
    const baseURL = await startGateway(headers);
    server = await serve({ chatClient: createGatewayClient({ baseURL, apiKey: 'k' }), model: 'm' });
    const client = await TestClient.connect(server.url);

    client.sendAction({
        action: 'create_conversation_session',
        requestId: 'cs-1',
        agentId: '11111111-1111-1111-1111-111111111111',
        userName: 'Test',
    });
    const created = await client.receive();
    const sessionId = (created.data as Record<string, unknown>).sessionId as string;

    client.sendAction({ action: 'send_message', requestId: 'sm-1', sessionId, message: 'How long can I return?' });
    await client.receive(); // 202 ack
    const { terminal } = await client.receiveUntil('eventual_response');
    await client.close();

    const inner = (terminal.data as Record<string, unknown>).data as Record<string, unknown>;
    return inner.usage as Record<string, number> | undefined;
}

afterEach(async () => {
    await server?.close();
    server = undefined;
    if (gateway) await new Promise<void>((resolve) => gateway!.close(() => resolve()));
    gateway = undefined;
});

describe('gateway cost reaches eventual_response.usage', () => {
    it('surfaces the header cost on the protocol frame', async () => {
        const usage = await turnUsage({ 'x-litellm-response-cost-margin-amount': '0.25' });

        expect(usage?.costUsd).toBe(0.25);
        // Token counts still come from the stream's usage chunk, unaffected.
        expect(usage?.promptTokens).toBe(10);
        expect(usage?.completionTokens).toBe(5);
    });

    it('takes the first NON-ZERO header, so a zero margin does not zero real spend', async () => {
        const usage = await turnUsage({
            'x-litellm-response-cost-margin-amount': '0',
            'x-litellm-response-cost-original': '0.5',
        });

        expect(usage?.costUsd).toBe(0.5);
    });

    it('treats an absent header and an all-zero header identically — both unmeasured', async () => {
        // The invariant is the EQUALITY: a present `0` must never be locked in as a real
        // $0; it falls through exactly as an absent header does, to the local estimate.
        // (Model `m` has no pricing entry, so that estimate is 0 here — the equality is
        // the assertion, not the number.)
        const absent = await turnUsage({});
        await server?.close();
        server = undefined;
        const allZero = await turnUsage({ 'x-litellm-response-cost': '0', 'x-cost-usd': '0' });

        expect(absent?.costUsd).toBe(allZero?.costUsd);
        // And emphatically not a value the gateway supplied in the tests above.
        expect(absent?.costUsd).not.toBe(0.25);
        expect(absent?.costUsd).not.toBe(0.5);
    });
});

describe('buildChatClient wires a header-reading client', () => {
    // The tests above inject the client directly, so they pin the server PIPELINE.
    // This one pins the WIRING — that main.ts hands the engine a client which can see
    // the cost header at all. Injecting the raw SDK (the bug) fails here and nowhere else.
    it('returns a streaming client whose chunks carry the gateway cost', async () => {
        const baseURL = await startGateway({ 'x-litellm-response-cost': '0.75' });
        const prior = { url: process.env.SMOOAI_GATEWAY_URL, key: process.env.SMOOAI_GATEWAY_KEY };
        process.env.SMOOAI_GATEWAY_URL = baseURL;
        process.env.SMOOAI_GATEWAY_KEY = 'k';

        try {
            const client = await buildChatClient();
            const stream = client.chat.completions.createStream?.({ model: 'm', messages: [{ role: 'user', content: 'hi' }] });
            expect(stream).toBeDefined();

            let cost: number | undefined;
            for await (const chunk of stream!) {
                if (chunk.gatewayCostUsd !== undefined) cost = chunk.gatewayCostUsd;
            }
            expect(cost).toBe(0.75);
        } finally {
            process.env.SMOOAI_GATEWAY_URL = prior.url;
            process.env.SMOOAI_GATEWAY_KEY = prior.key;
        }
    });
});
