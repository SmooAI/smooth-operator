/**
 * The Phase 1 headline, SDK-side: an in-process host drives the `hello` demo
 * extension through the full tool path — handshake → registration → execute →
 * streamed progress → cancellation.
 */
import { afterEach, describe, expect, it } from 'vitest';
import { z } from 'zod';
import { createTestHost, defineExtension, defineTool, RpcError, type TestHost } from '../src/index.js';
import { hello } from '../examples/hello.js';

let host: TestHost | undefined;
afterEach(() => host?.close());

describe('hello demo extension', () => {
    it('registers hello.greet (bare `greet`) at handshake', async () => {
        host = createTestHost(hello);
        const result = await host.initialize();
        expect(result.extension).toEqual({ name: 'hello', version: '0.1.0' });
        const tools = result.registrations?.tools ?? [];
        expect(tools.map((t) => t.name)).toContain('greet');
        // zod → JSON Schema landed on the wire.
        expect(tools[0]!.parameters).toMatchObject({ type: 'object', properties: { name: { type: 'string' } } });
        expect(result.registrations?.subscriptions).toContain('turn_start');
    });

    it('executes greet and returns the greeting', async () => {
        host = createTestHost(hello);
        await host.initialize();
        const res = await host.callTool('greet', { name: 'Ada' });
        expect(res).toEqual({ content: 'Hello, Ada!' });
    });

    it('streams a tool/update progress notification during execute', async () => {
        host = createTestHost(hello);
        await host.initialize();
        const updates: unknown[] = [];
        await host.callTool('greet', { name: 'Grace' }, { onUpdate: (u) => updates.push(u) });
        expect(updates).toHaveLength(1);
        expect(updates[0]).toMatchObject({ message: 'greeting Grace', progress: 0.5 });
    });
});

describe('tool cancellation', () => {
    it('$/cancel aborts an in-flight tool and rejects with -32800', async () => {
        // A tool that never resolves until aborted.
        const ext = defineExtension((smooth) => {
            smooth.name = 'slow';
            smooth.version = '0.0.1';
            smooth.registerTool(
                defineTool({
                    name: 'wait',
                    description: 'Wait until cancelled.',
                    parameters: z.object({}),
                    execute: (_args, ctx) =>
                        new Promise((_resolve, reject) => {
                            ctx.signal.addEventListener('abort', () => reject(new Error('aborted')), { once: true });
                        }),
                }),
            );
        });
        host = createTestHost(ext);
        await host.initialize();

        const controller = new AbortController();
        const pending = host.callTool('wait', {}, { signal: controller.signal });
        controller.abort();
        await expect(pending).rejects.toBeInstanceOf(RpcError);
        await expect(pending).rejects.toMatchObject({ code: -32800 });
    });
});

describe('unknown tool', () => {
    it('returns an error result rather than throwing', async () => {
        host = createTestHost(hello);
        await host.initialize();
        const res = await host.callTool('nope', {});
        expect(res.is_error).toBe(true);
        expect(res.content).toContain('unknown tool');
    });
});

describe('lifecycle', () => {
    it('answers ping and shutdown', async () => {
        host = createTestHost(hello);
        await host.initialize();
        await expect(host.ping()).resolves.toEqual({});
        await expect(host.shutdown()).resolves.toBeUndefined();
    });

    it('delivers subscribed events to on() handlers', async () => {
        const seen: string[] = [];
        const ext = defineExtension((smooth) => {
            smooth.name = 'watcher';
            smooth.version = '0.0.1';
            smooth.on('turn_start', (payload) => seen.push((payload?.turn_id as string) ?? '?'));
        });
        host = createTestHost(ext);
        await host.initialize();
        host.sendEvent('turn_start', { turn_id: 'turn-1' });
        // event is a notification delivered on a microtask; let it flush.
        await new Promise((r) => setTimeout(r, 5));
        expect(seen).toEqual(['turn-1']);
    });
});
