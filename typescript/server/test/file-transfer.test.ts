/**
 * File-transfer contract (PR #342) — the TS server's parity with Rust for
 * `send_message.files[]` / `images[]` and the `send_file` directive convention.
 *
 * Proven end-to-end over a real WebSocket:
 * - `images[]` reach the MODEL as OpenAI `image_url` content parts on the user
 *   message (via {@link withUserImages}), while the plain text drives retrieval;
 * - `files[]` are surfaced on the per-turn {@link ToolContext} to a host tool and
 *   are NEVER sent to the model;
 * - a host tool that writes `ctx.directive` has it drained onto
 *   `eventual_response.directive` (last-write-wins);
 * - malformed attachments are dropped fail-soft rather than failing the turn.
 *
 * Plus focused unit tests for the pure parsing/attach helpers.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import type { ChatClientLike, Tool } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { serve, type RunningServer } from '../src/server.js';
import { parseFiles, parseImages, withUserImages, type ToolContext, type ToolProvider, type UserFile } from '../src/toolContext.js';
import { TestClient } from './wsClient.js';

/** The last `role:'user'` message content across a recorded call's messages. */
function lastUserContent(messages: Array<Record<string, unknown>>): unknown {
    for (let i = messages.length - 1; i >= 0; i--) {
        const m = messages[i];
        if (m && m.role === 'user') return m.content;
    }
    return undefined;
}

async function createSession(client: TestClient): Promise<string> {
    client.sendAction({ action: 'create_conversation_session', requestId: 'cs' });
    return ((await client.receive()).data as Record<string, unknown>).sessionId as string;
}

describe('withUserImages (unit)', () => {
    const base: ChatClientLike = {
        chat: { completions: { create: async () => ({ choices: [{ message: { content: 'ok' } }] }) } },
    };

    it('returns the client unwrapped when there are no images (zero overhead)', () => {
        expect(withUserImages(base, [])).toBe(base);
    });

    it('rewrites the LAST user message content to [text, image_url...] parts', async () => {
        let seenBody: Record<string, unknown> | undefined;
        const client: ChatClientLike = {
            chat: {
                completions: {
                    create: async (body) => {
                        seenBody = body;
                        return { choices: [{ message: { content: 'ok' } }] };
                    },
                },
            },
        };
        const wrapped = withUserImages(client, [{ url: 'data:image/png;base64,AAAA', detail: 'high' }, { url: 'https://x/y.jpg' }]);
        await wrapped.chat.completions.create({
            messages: [
                { role: 'system', content: 'sys' },
                { role: 'user', content: 'earlier' },
                { role: 'assistant', content: 'reply' },
                { role: 'user', content: 'look at these' },
            ],
        });
        const content = lastUserContent(seenBody!.messages as Array<Record<string, unknown>>);
        expect(content).toEqual([
            { type: 'text', text: 'look at these' },
            { type: 'image_url', image_url: { url: 'data:image/png;base64,AAAA', detail: 'high' } },
            { type: 'image_url', image_url: { url: 'https://x/y.jpg' } },
        ]);
        // An EARLIER user message is left untouched — only the current turn's.
        expect((seenBody!.messages as Array<Record<string, unknown>>)[1].content).toBe('earlier');
    });

    it('does not mutate the caller-supplied body/messages', async () => {
        const client: ChatClientLike = { chat: { completions: { create: async () => ({ choices: [{ message: { content: 'ok' } }] }) } } };
        const wrapped = withUserImages(client, [{ url: 'https://x/y.jpg' }]);
        const original = { messages: [{ role: 'user', content: 'hi' }] };
        await wrapped.chat.completions.create(original);
        expect(original.messages[0].content).toBe('hi'); // untouched
    });
});

describe('parseImages / parseFiles (fail-soft)', () => {
    it('keeps valid images and a valid detail; drops the rest', () => {
        expect(
            parseImages([
                { url: 'https://a/1.png', detail: 'low' },
                { url: 'data:image/png;base64,AAAA' },
                { url: 'https://b/2.png', detail: 'bogus' }, // detail dropped, entry kept
                { url: '' }, // empty url dropped
                { detail: 'high' }, // no url dropped
                'nope', // non-object dropped
                null,
            ]),
        ).toEqual([{ url: 'https://a/1.png', detail: 'low' }, { url: 'data:image/png;base64,AAAA' }, { url: 'https://b/2.png' }]);
    });

    it('non-array ⇒ []', () => {
        expect(parseImages(undefined)).toEqual([]);
        expect(parseFiles('x')).toEqual([]);
    });

    it('keeps files with name+url (+optional mimeType); drops incomplete', () => {
        expect(
            parseFiles([
                { name: 'a.csv', url: 'data:text/csv;base64,AAAA', mimeType: 'text/csv' },
                { name: 'b.txt', url: 'https://x/b.txt' },
                { name: '', url: 'https://x/c' }, // empty name dropped
                { url: 'https://x/d' }, // no name dropped
                { name: 'e', mimeType: 'text/plain' }, // no url dropped
            ]),
        ).toEqual([{ name: 'a.csv', url: 'data:text/csv;base64,AAAA', mimeType: 'text/csv' }, { name: 'b.txt', url: 'https://x/b.txt' }]);
    });
});

describe('file-transfer server seam (integration)', () => {
    let server: RunningServer | undefined;

    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('images[] reach the model as image_url content parts on the user message', async () => {
        const chat = new MockLlmProvider().pushText('I see the image.');
        server = await serve({ chatClient: chat });
        const client = await TestClient.connect(server.url);
        const sessionId = await createSession(client);

        client.sendAction({
            action: 'send_message',
            requestId: 'sm',
            sessionId,
            message: 'what is this?',
            images: [{ url: 'data:image/png;base64,AAAA', detail: 'auto' }],
        });
        await client.receiveUntil('eventual_response');

        const content = lastUserContent(chat.calls[0].messages);
        expect(content).toEqual([
            { type: 'text', text: 'what is this?' },
            { type: 'image_url', image_url: { url: 'data:image/png;base64,AAAA', detail: 'auto' } },
        ]);
        await client.close();
    });

    it('a text-only turn keeps the user message as a plain string (byte-identical)', async () => {
        const chat = new MockLlmProvider().pushText('hi');
        server = await serve({ chatClient: chat });
        const client = await TestClient.connect(server.url);
        const sessionId = await createSession(client);

        client.sendAction({ action: 'send_message', requestId: 'sm', sessionId, message: 'plain question' });
        await client.receiveUntil('eventual_response');

        expect(lastUserContent(chat.calls[0].messages)).toBe('plain question');
        await client.close();
    });

    it('files[] surface on the tool context to a host tool and are NOT sent to the model', async () => {
        let seenFiles: UserFile[] | undefined;
        const provider: ToolProvider = (ctx: ToolContext) => [
            {
                name: 'ingest',
                description: 'Records the turn files.',
                parameters: { type: 'object', properties: {} },
                async execute() {
                    seenFiles = ctx.files;
                    return `saw ${ctx.files.length} file(s)`;
                },
            } satisfies Tool,
        ];

        const chat = new MockLlmProvider().pushToolCall('c1', 'ingest', '{}').pushText('done');
        server = await serve({ chatClient: chat, toolProvider: provider });
        const client = await TestClient.connect(server.url);
        const sessionId = await createSession(client);

        client.sendAction({
            action: 'send_message',
            requestId: 'sm',
            sessionId,
            message: 'here is a file',
            files: [{ name: 'data.csv', url: 'data:text/csv;base64,AAAA', mimeType: 'text/csv' }],
        });
        await client.receiveUntil('eventual_response');

        // The host tool saw the file on the context...
        expect(seenFiles).toEqual([{ name: 'data.csv', url: 'data:text/csv;base64,AAAA', mimeType: 'text/csv' }]);
        // ...and the bytes never went to the model (no message content mentions the data url).
        const anyModelSawFile = chat.calls.some((c) => JSON.stringify(c.messages).includes('data:text/csv'));
        expect(anyModelSawFile).toBe(false);
        await client.close();
    });

    it('a host tool that writes ctx.directive has it drained onto eventual_response.directive', async () => {
        const directive = { type: 'send_file', files: [{ name: 'out.pdf', url: 'data:application/pdf;base64,AAAA' }] };
        const provider: ToolProvider = (ctx: ToolContext) => [
            {
                name: 'send_file',
                description: 'Delivers a file to the user.',
                parameters: { type: 'object', properties: {} },
                async execute() {
                    ctx.directive = directive;
                    return 'delivered';
                },
            } satisfies Tool,
        ];

        const chat = new MockLlmProvider().pushToolCall('c1', 'send_file', '{}').pushText('Sent.');
        server = await serve({ chatClient: chat, toolProvider: provider });
        const client = await TestClient.connect(server.url);
        const sessionId = await createSession(client);

        client.sendAction({ action: 'send_message', requestId: 'sm', sessionId, message: 'send me the pdf' });
        const { terminal } = await client.receiveUntil('eventual_response');

        const inner = (terminal.data as Record<string, unknown>).data as Record<string, unknown>;
        expect(inner.directive).toEqual(directive);
        await client.close();
    });

    it('no directive written ⇒ eventual_response omits the field (back-compat)', async () => {
        const chat = new MockLlmProvider().pushText('hi');
        server = await serve({ chatClient: chat });
        const client = await TestClient.connect(server.url);
        const sessionId = await createSession(client);

        client.sendAction({ action: 'send_message', requestId: 'sm', sessionId, message: 'hello' });
        const { terminal } = await client.receiveUntil('eventual_response');

        const inner = (terminal.data as Record<string, unknown>).data as Record<string, unknown>;
        expect('directive' in inner).toBe(false);
        await client.close();
    });

    it('malformed attachments are dropped fail-soft — the turn still completes', async () => {
        let seenFiles: UserFile[] | undefined;
        let seenImageCount: number | undefined;
        const provider: ToolProvider = (ctx: ToolContext) => {
            seenFiles = ctx.files;
            seenImageCount = ctx.images.length;
            return [];
        };
        const chat = new MockLlmProvider().pushText('ok');
        server = await serve({ chatClient: chat, toolProvider: provider });
        const client = await TestClient.connect(server.url);
        const sessionId = await createSession(client);

        client.sendAction({
            action: 'send_message',
            requestId: 'sm',
            sessionId,
            message: 'mixed bag',
            images: [{ detail: 'high' }, 'garbage'], // both invalid → dropped
            files: [{ name: 'ok.txt', url: 'https://x/ok.txt' }, { url: 'https://x/no-name' }], // second dropped
        });
        const { terminal } = await client.receiveUntil('eventual_response');

        expect(terminal.status).toBe(200);
        expect(seenImageCount).toBe(0);
        expect(seenFiles).toEqual([{ name: 'ok.txt', url: 'https://x/ok.txt' }]);
        // No images parsed ⇒ user message stays a plain string.
        expect(lastUserContent(chat.calls[0].messages)).toBe('mixed bag');
        await client.close();
    });
});
