/**
 * Boot test: `serveLocal()` starts an in-memory, auth-off server and accepts a real
 * WebSocket connection (ping → pong). The TS parity of the C# host smoke test.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { serveLocal, type RunningServer } from '../src/server.js';
import { TestClient } from './wsClient.js';

describe('boot', () => {
    let server: RunningServer | undefined;

    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('serveLocal starts on an ephemeral port and accepts a connection', async () => {
        server = await serveLocal({ chatClient: new MockLlmProvider() });
        expect(server.port).toBeGreaterThan(0);
        expect(server.url).toMatch(/^ws:\/\/127\.0\.0\.1:\d+\/ws$/);

        const client = await TestClient.connect(server.url);
        client.sendAction({ action: 'ping', requestId: 'ping-1' });
        const pong = await client.receive();
        expect(pong.type).toBe('pong');
        expect(pong.requestId).toBe('ping-1');
        await client.close();
    });

    it('unknown action errors without dropping the connection', async () => {
        server = await serveLocal({ chatClient: new MockLlmProvider() });
        const client = await TestClient.connect(server.url);

        client.sendAction({ action: 'frobnicate', requestId: 'x1' });
        const error = await client.receive();
        expect(error.type).toBe('error');

        // The connection survives — a subsequent ping still works.
        client.sendAction({ action: 'ping', requestId: 'ping-2' });
        const pong = await client.receive();
        expect(pong.type).toBe('pong');
        expect(pong.requestId).toBe('ping-2');
        await client.close();
    });

    it('an invalid JSON frame errors but keeps the connection alive', async () => {
        server = await serveLocal({ chatClient: new MockLlmProvider() });
        const client = await TestClient.connect(server.url);

        client.send('{not json');
        const error = await client.receive();
        expect(error.type).toBe('error');
        expect(((error.data as Record<string, unknown>).error as Record<string, unknown>).code).toBe('VALIDATION_ERROR');

        client.sendAction({ action: 'ping', requestId: 'after-bad' });
        const pong = await client.receive();
        expect(pong.type).toBe('pong');
        await client.close();
    });
});
