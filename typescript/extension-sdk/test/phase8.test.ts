/**
 * Phase 8 SDK surface: render-block builders, the inter-extension bus
 * (`events.publish`/`events.on`), declarative message renderers, the `context`
 * hook, and `widget/key` routing — all driven through `createTestHost`.
 */
import { afterEach, describe, expect, it } from 'vitest';
import { z } from 'zod';
import { createTestHost, defineExtension, defineTool, render, type TestHost } from '../src/index.js';

let host: TestHost | undefined;
afterEach(() => host?.close());

describe('render-block builders', () => {
    it('produce the wire shapes', () => {
        expect(render.markdown('hi')).toEqual({ kind: 'markdown', text: 'hi' });
        expect(render.progress(0.5, { label: 'x' })).toEqual({ kind: 'progress', value: 0.5, label: 'x' });
        const w = render.widget('snake', render.markdown('board'), [{ key: 'ArrowUp' }], { text: 'snake' });
        expect(w).toEqual({
            kind: 'widget',
            widget_id: 'snake',
            body: { kind: 'markdown', text: 'board' },
            keybindings: [{ key: 'ArrowUp' }],
            text: 'snake',
        });
    });
});

describe('inter-extension bus', () => {
    it('events.publish reaches the host as bus/publish', async () => {
        const ext = defineExtension((smooth) => {
            smooth.name = 'pub';
            smooth.version = '0.0.1';
            smooth.registerTool(
                defineTool({
                    name: 'shout',
                    description: 'Publish on the bus.',
                    parameters: z.object({}),
                    execute() {
                        smooth.events.publish('greeting', { hello: 'world' });
                        return { content: 'ok' };
                    },
                }),
            );
        });
        host = createTestHost(ext);
        await host.initialize();
        await host.callTool('shout', {});
        expect(host.busPublishes).toEqual([{ topic: 'greeting', payload: { hello: 'world' } }]);
    });

    it('events.on subscribes to bus/event filtered by topic', async () => {
        const got: { payload: unknown; from: string }[] = [];
        const ext = defineExtension((smooth) => {
            smooth.name = 'sub';
            smooth.version = '0.0.1';
            smooth.events.on('greeting', (payload, from) => got.push({ payload, from }));
            smooth.events.on('other', () => got.push({ payload: 'WRONG', from: 'x' }));
        });
        host = createTestHost(ext);
        const result = await host.initialize();
        // Subscribing to a topic puts bus/event in subscriptions.
        expect(result.registrations?.subscriptions).toContain('bus/event');

        host.sendEvent('bus/event', { from: 'pub', topic: 'greeting', payload: { hello: 'world' } });
        host.sendEvent('bus/event', { from: 'pub', topic: 'unrelated', payload: { n: 1 } });
        await Promise.resolve();
        expect(got).toEqual([{ payload: { hello: 'world' }, from: 'pub' }]);
    });
});

describe('declarative message renderers', () => {
    it('are reported in the registrations', async () => {
        const ext = defineExtension((smooth) => {
            smooth.name = 'rend';
            smooth.version = '0.0.1';
            smooth.registerMessageRenderer('snake_board', render.markdown('score {{score}}'));
        });
        host = createTestHost(ext);
        const result = await host.initialize();
        expect(result.registrations?.message_renderers).toEqual([{ tag: 'snake_board', template: { kind: 'markdown', text: 'score {{score}}' } }]);
    });
});

describe('context hook + hooks declaration', () => {
    it('declares the context hook and can replace the message array', async () => {
        const ext = defineExtension((smooth) => {
            smooth.name = 'ctx';
            smooth.version = '0.0.1';
            smooth.on('context', () => ({ patch: { messages: [{ role: 'user', content: 'REPLACED' }] } }));
        });
        host = createTestHost(ext);
        const result = await host.initialize();
        expect(result.registrations?.hooks).toContain('context');

        const outcome = await host.callHook('context', { messages: [{ role: 'user', content: 'original' }] });
        expect(outcome).toEqual({ action: 'modify', patch: { messages: [{ role: 'user', content: 'REPLACED' }] } });
    });
});

describe('widget/key routing', () => {
    it('delivers a widget/key event to the on handler', async () => {
        const keys: string[] = [];
        const ext = defineExtension((smooth) => {
            smooth.name = 'game';
            smooth.version = '0.0.1';
            smooth.on('widget/key', (payload) => {
                keys.push((payload as { key: string }).key);
            });
        });
        host = createTestHost(ext);
        const result = await host.initialize();
        expect(result.registrations?.subscriptions).toContain('widget/key');

        host.sendEvent('widget/key', { widget_id: 'snake', key: 'ArrowLeft' });
        await Promise.resolve();
        expect(keys).toEqual(['ArrowLeft']);
    });
});
