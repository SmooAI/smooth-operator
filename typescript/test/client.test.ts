/**
 * Client behaviour, driven entirely through a mock transport — no live server.
 *
 * Covers the core streaming contract: a `send_message` turn surfaces
 * `stream_token` → `stream_chunk` → `eventual_response` as typed events in arrival
 * order, and resolves the turn with the terminal response. Also covers request
 * correlation for non-streaming actions, error propagation, and HITL resume.
 */
import { describe, expect, it, vi } from 'vitest';
import { SmoothAgentClient, ProtocolError, type MessageTurn } from '../src/client.js';
import type { Transport, TransportState } from '../src/transport.js';
import type { WebSocketLike } from '../src/transport.js';
import type { ServerEvent } from '../src/types.js';

/** In-memory transport: captures sent frames, lets the test inject server events. */
class MockTransport implements Transport {
    state: TransportState = 'closed';
    readonly sent: string[] = [];
    private messageHandlers = new Set<(data: string) => void>();
    private closeHandlers = new Set<(info: { code?: number; reason?: string }) => void>();
    private errorHandlers = new Set<(err: unknown) => void>();

    connect(): Promise<void> {
        this.state = 'open';
        return Promise.resolve();
    }
    send(data: string): void {
        if (this.state !== 'open') throw new Error(`not open: ${this.state}`);
        this.sent.push(data);
    }
    close(): void {
        this.state = 'closed';
        for (const h of this.closeHandlers) h({ code: 1000 });
    }
    onMessage(handler: (data: string) => void): () => void {
        this.messageHandlers.add(handler);
        return () => this.messageHandlers.delete(handler);
    }
    onClose(handler: (info: { code?: number; reason?: string }) => void): () => void {
        this.closeHandlers.add(handler);
        return () => this.closeHandlers.delete(handler);
    }
    onError(handler: (err: unknown) => void): () => void {
        this.errorHandlers.add(handler);
        return () => this.errorHandlers.delete(handler);
    }

    /** Simulate a server→client event. */
    emit(event: ServerEvent): void {
        const data = JSON.stringify(event);
        for (const h of this.messageHandlers) h(data);
    }

    /** The last action frame the client sent, parsed. */
    lastSent<T = Record<string, unknown>>(): T {
        return JSON.parse(this.sent.at(-1)!) as T;
    }
}

function makeClient(): { client: SmoothAgentClient; transport: MockTransport } {
    const transport = new MockTransport();
    let counter = 0;
    const client = new SmoothAgentClient({
        url: 'wss://test',
        transport,
        generateRequestId: () => `req-test-${++counter}`,
        requestTimeout: 1000,
    });
    return { client, transport };
}

describe('SmoothAgentClient.sendMessage streaming', () => {
    it('surfaces stream_token then stream_chunk then eventual_response in order, and resolves', async () => {
        const { client, transport } = makeClient();
        await client.connect();

        const turn = client.sendMessage({ sessionId: 'sess-1', message: 'hi', stream: true });
        const reqId = transport.lastSent<{ requestId: string }>().requestId;

        // The outgoing frame is a well-formed send_message action.
        expect(transport.lastSent()).toMatchObject({ action: 'send_message', sessionId: 'sess-1', message: 'hi' });

        // Collect streamed events via async iteration in a background task.
        const collected: ServerEvent[] = [];
        const iterate = (async () => {
            for await (const ev of turn) collected.push(ev);
        })();

        // Drive a realistic event sequence.
        transport.emit({ type: 'stream_token', requestId: reqId, token: 'Hel', data: { requestId: reqId, token: 'Hel' } });
        transport.emit({ type: 'stream_token', requestId: reqId, token: 'lo', data: { requestId: reqId, token: 'lo' } });
        transport.emit({
            type: 'stream_chunk',
            requestId: reqId,
            node: 'response_composer',
            data: {
                requestId: reqId,
                node: 'response_composer',
                state: { structuredResponse: { responseParts: ['Hello'] } },
            },
        });
        transport.emit({
            type: 'eventual_response',
            requestId: reqId,
            status: 200,
            data: {
                requestId: reqId,
                status: 200,
                data: { messageId: 'msg-1', response: { responseParts: ['Hello'] }, needsEscalation: false },
            },
        });

        const final = await turn;
        await iterate;

        // Terminal response resolves the turn.
        expect(final.type).toBe('eventual_response');
        expect(final.data.data.messageId).toBe('msg-1');

        // Events arrived in order through iteration.
        expect(collected.map((e) => e.type)).toEqual([
            'stream_token',
            'stream_token',
            'stream_chunk',
            'eventual_response',
        ]);
        const tokens = collected.filter((e) => e.type === 'stream_token').map((e) => (e as { token?: string }).token);
        expect(tokens.join('')).toBe('Hello');
    });

    it('accumulates tokens that are pushed before iteration begins (buffering)', async () => {
        const { client, transport } = makeClient();
        await client.connect();

        const turn = client.sendMessage({ sessionId: 's', message: 'q' });
        const reqId = transport.lastSent<{ requestId: string }>().requestId;

        // Emit before anyone iterates — must be buffered.
        transport.emit({ type: 'stream_token', requestId: reqId, token: 'A', data: { requestId: reqId, token: 'A' } });
        transport.emit({
            type: 'eventual_response',
            requestId: reqId,
            status: 200,
            data: { requestId: reqId, status: 200, data: { messageId: 'm', response: null } },
        });

        const types: string[] = [];
        for await (const ev of turn) types.push(ev.type);
        expect(types).toEqual(['stream_token', 'eventual_response']);
    });

    it('rejects the turn on an error event with a ProtocolError', async () => {
        const { client, transport } = makeClient();
        await client.connect();
        const turn = client.sendMessage({ sessionId: 's', message: 'boom' });
        const reqId = transport.lastSent<{ requestId: string }>().requestId;

        transport.emit({
            type: 'error',
            requestId: reqId,
            data: { requestId: reqId, error: { code: 'RATE_LIMITED', message: 'slow down' } },
        });

        await expect(turn).rejects.toBeInstanceOf(ProtocolError);
        await expect(turn).rejects.toMatchObject({ code: 'RATE_LIMITED' });
    });

    it('routes a HITL confirm resume back into the same turn', async () => {
        const { client, transport } = makeClient();
        await client.connect();
        const turn = client.sendMessage({ sessionId: 's', message: 'delete it' });
        const reqId = transport.lastSent<{ requestId: string }>().requestId;

        const seen: string[] = [];
        const iterate = (async () => {
            for await (const ev of turn) seen.push(ev.type);
        })();

        transport.emit({
            type: 'write_confirmation_required',
            requestId: reqId,
            data: { requestId: reqId, data: { toolId: 't1', actionDescription: 'Delete contact' } },
        });

        // Caller approves; the resumed stream completes the original turn.
        client.confirmToolAction({ sessionId: 's', requestId: reqId, approved: true });
        expect(transport.lastSent()).toMatchObject({ action: 'confirm_tool_action', approved: true, requestId: reqId });

        transport.emit({
            type: 'eventual_response',
            requestId: reqId,
            status: 200,
            data: { requestId: reqId, status: 200, data: { messageId: 'm', response: null } },
        });

        await turn;
        await iterate;
        expect(seen).toEqual(['write_confirmation_required', 'eventual_response']);
    });

    it('routes a Rich Interaction park through invalid → valid resubmit back into the same turn', async () => {
        const { client, transport } = makeClient();
        await client.connect();
        const turn = client.sendMessage({ sessionId: 's', message: 'quote please' });
        const reqId = transport.lastSent<{ requestId: string }>().requestId;

        const seen: string[] = [];
        const iterate = (async () => {
            for await (const ev of turn) seen.push(ev.type);
        })();

        transport.emit({
            type: 'interaction_required',
            requestId: reqId,
            data: {
                requestId: reqId,
                data: {
                    interactionId: 'int-1',
                    kind: 'identity_intake',
                    spec: { fields: [{ key: 'email', required: true }] },
                    reason: 'to send you the quote',
                },
            },
        });

        // First submit fails server-side validation — the turn stays parked and
        // the invalid event flows into the SAME turn (never a terminal error).
        client.submitInteraction({ sessionId: 's', requestId: reqId, interactionId: 'int-1', values: { email: 'nope' } });
        expect(transport.lastSent()).toMatchObject({
            action: 'submit_interaction',
            requestId: reqId,
            interactionId: 'int-1',
            values: { email: 'nope' },
        });
        transport.emit({
            type: 'interaction_invalid',
            requestId: reqId,
            data: {
                requestId: reqId,
                data: {
                    interactionId: 'int-1',
                    kind: 'identity_intake',
                    errors: [{ field: 'email', message: 'must be a valid email address' }],
                    message: 'Some fields need attention.',
                },
            },
        });

        // Resubmit with valid values; the resumed stream completes the turn.
        client.submitInteraction({ sessionId: 's', requestId: reqId, interactionId: 'int-1', values: { email: 'a@b.co' } });
        transport.emit({
            type: 'eventual_response',
            requestId: reqId,
            status: 200,
            data: { requestId: reqId, status: 200, data: { messageId: 'm', response: null } },
        });

        await turn;
        await iterate;
        expect(seen).toEqual(['interaction_required', 'interaction_invalid', 'eventual_response']);
    });

    it('submitInteraction can decline', async () => {
        const { client, transport } = makeClient();
        await client.connect();
        client.submitInteraction({ sessionId: 's', requestId: 'r1', interactionId: 'int-9', declined: true });
        expect(transport.lastSent()).toMatchObject({
            action: 'submit_interaction',
            sessionId: 's',
            requestId: 'r1',
            interactionId: 'int-9',
            declined: true,
        });
    });
});

describe('SmoothAgentClient request correlation', () => {
    it('createConversationSession resolves with the immediate_response data', async () => {
        const { client, transport } = makeClient();
        await client.connect();

        const promise = client.createConversationSession({ agentId: 'agent-1', userName: 'Alice' });
        const reqId = transport.lastSent<{ requestId: string }>().requestId;
        expect(transport.lastSent()).toMatchObject({ action: 'create_conversation_session', agentId: 'agent-1' });

        transport.emit({
            type: 'immediate_response',
            requestId: reqId,
            status: 200,
            data: {
                sessionId: 'sess-9',
                conversationId: 'conv-9',
                agentId: 'agent-1',
                agentName: 'Aria',
                userParticipantId: 'u-9',
                agentParticipantId: 'a-9',
            },
        });

        const session = await promise;
        expect(session).toMatchObject({ sessionId: 'sess-9', agentName: 'Aria' });
    });

    it('createConversationSession forwards conversationId for resume', async () => {
        const { client, transport } = makeClient();
        await client.connect();

        const promise = client.createConversationSession({ agentId: 'agent-1', conversationId: 'conv-existing' });
        const reqId = transport.lastSent<{ requestId: string }>().requestId;
        expect(transport.lastSent()).toMatchObject({ action: 'create_conversation_session', conversationId: 'conv-existing' });

        transport.emit({
            type: 'immediate_response',
            requestId: reqId,
            status: 200,
            data: { sessionId: 's-r', conversationId: 'conv-existing', agentId: 'agent-1', agentName: 'Aria', userParticipantId: 'u', agentParticipantId: 'a' },
        });
        await expect(promise).resolves.toMatchObject({ conversationId: 'conv-existing' });
    });

    it('listConversations resolves with the conversation rows', async () => {
        const { client, transport } = makeClient();
        await client.connect();

        const promise = client.listConversations({ limit: 10 });
        const reqId = transport.lastSent<{ requestId: string }>().requestId;
        expect(transport.lastSent()).toMatchObject({ action: 'list_conversations', limit: 10 });

        transport.emit({
            type: 'immediate_response',
            requestId: reqId,
            status: 200,
            data: {
                conversations: [
                    { conversationId: 'c1', title: 'Returns policy', updatedAt: '2026-07-09T00:00:00Z', messageCount: 4 },
                    { conversationId: 'c2', title: 'Shipping', updatedAt: '2026-07-08T00:00:00Z', messageCount: 2 },
                ],
            },
        });

        const res = await promise;
        expect(res.conversations).toHaveLength(2);
        expect(res.conversations[0]).toMatchObject({ conversationId: 'c1', messageCount: 4 });
    });

    it('ping resolves with the pong timestamp', async () => {
        const { client, transport } = makeClient();
        await client.connect();
        const promise = client.ping();
        const reqId = transport.lastSent<{ requestId: string }>().requestId;
        transport.emit({ type: 'pong', requestId: reqId, timestamp: 1_700_000_000_000 });
        await expect(promise).resolves.toBe(1_700_000_000_000);
    });

    it('does not cross-correlate two concurrent requests', async () => {
        const { client, transport } = makeClient();
        await client.connect();

        const p1 = client.getSession({ sessionId: 's1' });
        const req1 = transport.lastSent<{ requestId: string }>().requestId;
        const p2 = client.getSession({ sessionId: 's2' });
        const req2 = transport.lastSent<{ requestId: string }>().requestId;
        expect(req1).not.toBe(req2);

        const sessionData = (id: string) => ({
            sessionId: id,
            conversationId: 'c',
            agentId: 'a',
            agentName: 'N',
            userParticipantId: 'u',
            agentParticipantId: 'ag',
        });

        // Resolve out of order.
        transport.emit({ type: 'immediate_response', requestId: req2, status: 200, data: sessionData('s2') });
        transport.emit({ type: 'immediate_response', requestId: req1, status: 200, data: sessionData('s1') });

        expect((await p1).sessionId).toBe('s1');
        expect((await p2).sessionId).toBe('s2');
    });

    it('forwards uncorrelated keepalive events to onEvent listeners', async () => {
        const { client, transport } = makeClient();
        await client.connect();
        const spy = vi.fn();
        client.onEvent(spy);
        transport.emit({ type: 'keepalive', data: { requestId: 'whatever' } });
        expect(spy).toHaveBeenCalledTimes(1);
        expect(spy.mock.calls[0]![0]).toMatchObject({ type: 'keepalive' });
    });

    it('rejects pending requests when the transport closes', async () => {
        const { client, transport } = makeClient();
        await client.connect();
        const promise = client.getSession({ sessionId: 's' });
        transport.close();
        await expect(promise).rejects.toBeInstanceOf(Error);
    });
});

describe('SmoothAgentClient connection token (?token=)', () => {
    /** A no-op WebSocket stub good enough for the default transport to dial. */
    function stubSocket(): WebSocketLike {
        return {
            readyState: 0,
            send: () => {},
            close: () => {},
            addEventListener: () => {},
        } as unknown as WebSocketLike;
    }

    /** Capture the URL the default transport hands to its WebSocket factory. */
    function urlForOptions(opts: { url: string; token?: string }): string {
        let dialed = '';
        // The factory is only invoked on connect(); constructing the client wires it up.
        const client = new SmoothAgentClient({
            ...opts,
            webSocketFactory: (u: string) => {
                dialed = u;
                return stubSocket();
            },
        });
        // Kick the transport so the factory runs and records the URL. The stub never
        // reaches OPEN, so connect() won't resolve — we don't await it, we only need
        // the synchronous factory call it triggers.
        void client.connect();
        return dialed;
    }

    it('appends token to a URL with no existing query string', () => {
        const dialed = urlForOptions({ url: 'wss://local.smooth-agent.dev/ws', token: 'secret123' });
        expect(new URL(dialed).searchParams.get('token')).toBe('secret123');
        expect(dialed).toContain('token=secret123');
    });

    it('preserves an existing query string and appends token after it', () => {
        const dialed = urlForOptions({ url: 'wss://local.smooth-agent.dev/ws?foo=bar', token: 'secret123' });
        const parsed = new URL(dialed);
        expect(parsed.searchParams.get('foo')).toBe('bar');
        expect(parsed.searchParams.get('token')).toBe('secret123');
        expect(dialed).toBe('wss://local.smooth-agent.dev/ws?foo=bar&token=secret123');
    });

    it('percent-encodes a token with reserved characters', () => {
        const dialed = urlForOptions({ url: 'wss://local.smooth-agent.dev/ws', token: 'a b&c=d' });
        // Round-trips back to the raw token, and the reserved `&`/`=` are encoded so
        // they don't spawn phantom query params.
        expect(new URL(dialed).searchParams.get('token')).toBe('a b&c=d');
        expect(dialed).toContain('token=a+b%26c%3Dd');
    });

    it('leaves the URL byte-for-byte unchanged when no token is given', () => {
        const url = 'wss://local.smooth-agent.dev/ws?foo=bar';
        expect(urlForOptions({ url })).toBe(url);
    });
});
