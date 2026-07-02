/** The JSON-RPC leaf: request/reply, notifications, and unknown-method errors. */
import { describe, expect, it } from 'vitest';
import { Peer, RpcError, linkedPair } from '../src/index.js';

/** Wire two peers over a linked in-memory transport pair. */
function connectedPeers(): [Peer, Peer] {
    const [ta, tb] = linkedPair();
    const a = new Peer({ send: (f) => ta.send(f) });
    const b = new Peer({ send: (f) => tb.send(f) });
    ta.start((f) => a.receive(f));
    tb.start((f) => b.receive(f));
    return [a, b];
}

describe('Peer', () => {
    it('round-trips a request to a handler', async () => {
        const [a, b] = connectedPeers();
        b.setRequestHandler('add', (params) => {
            const { x, y } = params as { x: number; y: number };
            return { sum: x + y };
        });
        await expect(a.request('add', { x: 2, y: 3 })).resolves.toEqual({ sum: 5 });
    });

    it('rejects an unknown method with -32601', async () => {
        const [a] = connectedPeers();
        await expect(a.request('bogus', {})).rejects.toBeInstanceOf(RpcError);
        await expect(a.request('bogus', {})).rejects.toMatchObject({ code: -32601 });
    });

    it('delivers notifications without a reply', async () => {
        const [a, b] = connectedPeers();
        const got: unknown[] = [];
        b.setNotificationHandler('log', (p) => got.push(p));
        a.notify('log', { level: 'info' });
        await new Promise((r) => setTimeout(r, 5));
        expect(got).toEqual([{ level: 'info' }]);
    });

    it('surfaces a handler-thrown RpcError with its code', async () => {
        const [a, b] = connectedPeers();
        b.setRequestHandler('boom', () => {
            throw new RpcError(-32002, 'not trusted');
        });
        await expect(a.request('boom', {})).rejects.toMatchObject({ code: -32002, message: 'not trusted' });
    });
});
