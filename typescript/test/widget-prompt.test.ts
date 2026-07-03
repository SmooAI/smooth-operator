// SEP Phase 6 — chat-widget button prompts. Drives the ConversationController
// through a `write_confirmation_required` HITL turn and asserts it renders a
// chat-native prompt, that a button click sends `confirm_tool_action` and
// resumes the turn, and that the prompt records the choice.
import { describe, expect, it } from 'vitest';

import type { Transport, TransportState } from '../src/transport.js';
import type { ServerEvent } from '../src/types.js';
import { type ChatMessage, ConversationController } from '../src/widget/conversation.js';

/** In-memory transport: captures sent frames, lets the test inject events. */
class MockTransport implements Transport {
    state: TransportState = 'closed';
    readonly sent: string[] = [];
    private handlers = new Set<(data: string) => void>();

    connect(): Promise<void> {
        this.state = 'open';
        return Promise.resolve();
    }
    send(data: string): void {
        this.sent.push(data);
    }
    close(): void {
        this.state = 'closed';
    }
    onMessage(h: (data: string) => void): () => void {
        this.handlers.add(h);
        return () => this.handlers.delete(h);
    }
    onClose(): () => void {
        return () => {};
    }
    onError(): () => void {
        return () => {};
    }
    emit(event: ServerEvent): void {
        const data = JSON.stringify(event);
        for (const h of this.handlers) h(data);
    }
    lastSent<T = Record<string, unknown>>(): T {
        return JSON.parse(this.sent.at(-1)!) as T;
    }
}

const tick = () => new Promise((r) => setTimeout(r, 0));

function makeController() {
    const transport = new MockTransport();
    let counter = 0;
    let latest: ChatMessage[] = [];
    const controller = new ConversationController(
        { endpoint: 'wss://test', agentId: 'a' },
        {
            onMessages: (m) => {
                latest = m;
            },
            onStatus: () => {},
        },
        { transport, generateRequestId: () => `req-${++counter}`, requestTimeout: 1000 },
    );
    return { controller, transport, messages: () => latest };
}

describe('widget confirm prompt (SEP ui/confirm)', () => {
    it('renders Yes/No buttons on write_confirmation_required and resumes on a click', async () => {
        const { controller, transport, messages } = makeController();

        // Don't await connect() before acking — it awaits the session response,
        // which we emit by hand below.
        const connected = controller.connect();
        await tick();
        const createReq = transport.lastSent<{ requestId: string }>().requestId;
        transport.emit({
            type: 'immediate_response',
            requestId: createReq,
            status: 200,
            data: { sessionId: 's', conversationId: 'c', agentId: 'a', agentName: 'N', userParticipantId: 'u', agentParticipantId: 'ag' },
        } as unknown as ServerEvent);
        await connected;

        // Fire a turn but don't await — we drive its events by hand.
        const done = controller.send('delete it');
        await tick();
        const msgReq = transport.lastSent<{ requestId: string }>().requestId;

        // Server pauses asking for confirmation.
        transport.emit({
            type: 'write_confirmation_required',
            requestId: msgReq,
            data: { requestId: msgReq, data: { toolId: 't1', actionDescription: 'Delete contact John' } },
        } as unknown as ServerEvent);
        await tick();

        // A prompt bubble is now present with two buttons and no answer yet.
        const prompt = messages().find((m) => m.prompt)?.prompt;
        expect(prompt).toBeTruthy();
        expect(prompt!.kind).toBe('confirm');
        expect(prompt!.text).toBe('Delete contact John');
        expect(prompt!.options.map((o) => o.label)).toEqual(['Yes', 'No']);
        expect(prompt!.answered).toBeUndefined();

        // User clicks Yes → confirm_tool_action goes out, prompt records the choice.
        controller.answerPrompt(msgReq, true);
        await tick();
        expect(transport.lastSent()).toMatchObject({ action: 'confirm_tool_action', approved: true, requestId: msgReq });
        expect(messages().find((m) => m.prompt)?.prompt?.answered).toBe('Yes');

        // Resumed stream completes the original turn.
        transport.emit({
            type: 'eventual_response',
            requestId: msgReq,
            status: 200,
            data: { requestId: msgReq, status: 200, data: { messageId: 'm', response: null } },
        } as unknown as ServerEvent);
        await done;
    });

    it('answerPrompt is a no-op for an unknown requestId', () => {
        const { controller, transport } = makeController();
        controller.answerPrompt('nope', true);
        expect(transport.sent).toHaveLength(0);
    });
});
