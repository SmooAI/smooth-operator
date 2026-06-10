/**
 * Tests for the headless hook + the SmoothChat component, driven end-to-end over
 * a mock transport — no live server, exactly like the protocol client's own
 * unit tests. We assert the real streaming path: tokens accumulate, the terminal
 * response finalizes, and citations attach.
 */
import { SmoothAgentClient, type Transport } from '../../src/index.js';
import { act, fireEvent, render, renderHook, screen, waitFor } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { SmoothChat } from '../../src/react/components/SmoothChat.js';
import { useConversation } from '../../src/react/use-conversation.js';

/**
 * A scripted in-memory transport. It records sent frames and, via `autoRespond`,
 * synthesizes the server's reply (session ack, streamed tokens, terminal
 * response) so the whole flow is deterministic.
 */
class MockTransport implements Transport {
    state: Transport['state'] = 'closed';
    readonly sent: any[] = [];
    private messageHandlers = new Set<(data: string) => void>();
    private closeHandlers = new Set<(info: { code?: number; reason?: string }) => void>();

    async connect(): Promise<void> {
        this.state = 'open';
    }
    close(): void {
        this.state = 'closed';
        for (const h of this.closeHandlers) h({ code: 1000 });
    }
    send(data: string): void {
        const frame = JSON.parse(data);
        this.sent.push(frame);
        this.autoRespond(frame);
    }
    onMessage(handler: (data: string) => void): () => void {
        this.messageHandlers.add(handler);
        return () => this.messageHandlers.delete(handler);
    }
    onClose(handler: (info: { code?: number; reason?: string }) => void): () => void {
        this.closeHandlers.add(handler);
        return () => this.closeHandlers.delete(handler);
    }
    onError(): () => void {
        return () => {};
    }

    receive(obj: unknown): void {
        const data = JSON.stringify(obj);
        for (const h of this.messageHandlers) h(data);
    }

    private autoRespond(frame: any): void {
        if (frame.action === 'create_conversation_session') {
            this.receive({ type: 'immediate_response', requestId: frame.requestId, status: 200, data: { sessionId: 'sess-1', conversationId: 'conv-1' } });
        } else if (frame.action === 'send_message') {
            this.receive({ type: 'stream_token', requestId: frame.requestId, token: 'Hello' });
            this.receive({ type: 'stream_token', requestId: frame.requestId, token: ' world' });
            this.receive({
                type: 'eventual_response',
                requestId: frame.requestId,
                status: 200,
                data: {
                    messageId: 'msg-1',
                    data: {
                        response: { responseParts: ['Hello world'] },
                        citations: [{ id: 'doc-1', title: 'Runbook', snippet: 'the relevant bit', score: 0.92, url: 'https://docs.test/runbook' }],
                    },
                },
            });
        }
    }
}

async function connectedClient(): Promise<{ client: SmoothAgentClient; transport: MockTransport }> {
    const transport = new MockTransport();
    const client = new SmoothAgentClient({ url: 'ws://test/ws', transport });
    await client.connect();
    return { client, transport };
}

describe('useConversation', () => {
    it('connects, streams tokens, finalizes, and attaches citations', async () => {
        const { client } = await connectedClient();
        const { result } = renderHook(() => useConversation({ client, agentId: 'agent-1' }));

        await waitFor(() => expect(result.current.status).toBe('ready'));

        await act(async () => {
            await result.current.send('hi there');
        });

        const { messages } = result.current;
        expect(messages.map((m) => m.role)).toEqual(['user', 'assistant']);
        expect(messages[0]?.text).toBe('hi there');
        expect(messages[1]?.text).toBe('Hello world');
        expect(messages[1]?.streaming).toBe(false);
        expect(messages[1]?.citations?.[0]?.title).toBe('Runbook');
        expect(messages[1]?.citations?.[0]?.url).toBe('https://docs.test/runbook');
    });

    it('sends the agentId on session creation', async () => {
        const { client, transport } = await connectedClient();
        const { result } = renderHook(() => useConversation({ client, agentId: 'agent-xyz' }));
        await waitFor(() => expect(result.current.status).toBe('ready'));

        const createFrame = transport.sent.find((f) => f.action === 'create_conversation_session');
        expect(createFrame?.agentId).toBe('agent-xyz');
    });
});

describe('<SmoothChat>', () => {
    it('renders the greeting, then the exchanged messages', async () => {
        const { client } = await connectedClient();
        render(<SmoothChat client={client} agentId="agent-1" agentName="Support" greeting="How can I help?" />);

        expect(screen.getByText('Support')).toBeDefined();
        await waitFor(() => expect(screen.getByText('How can I help?')).toBeDefined());

        const input = screen.getByLabelText('Message') as HTMLTextAreaElement;
        fireEvent.change(input, { target: { value: 'hello' } });
        await act(async () => {
            fireEvent.click(screen.getByText('Send'));
        });

        await waitFor(() => expect(screen.getByText('hello')).toBeDefined());
        await waitFor(() => expect(screen.getByText('Hello world')).toBeDefined());
    });
});
