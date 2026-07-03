/**
 * The Phase 3 UI surface, SDK-side: `hasUI` gating reflects the handshake's
 * `ui_capabilities`, `ctx.ui.*` round-trips `ui/request` to the host, and a
 * headless host answers -32001 NoUI. Driven through the `todo` demo extension.
 */
import { afterEach, describe, expect, it } from 'vitest';
import { z } from 'zod';
import { createTestHost, defineExtension, defineTool, errorCode, RpcError, type TestHost, type UiRequestParams } from '../src/index.js';
import { createTodo } from '../examples/todo.js';

let host: TestHost | undefined;
afterEach(() => host?.close());

describe('hasUI gating', () => {
    it('reflects the ui_capabilities the host declared at initialize', async () => {
        const seen: string[] = [];
        const ext = defineExtension((smooth) => {
            smooth.name = 'probe';
            smooth.version = '0.0.1';
            smooth.registerTool(
                defineTool({
                    name: 'probe',
                    description: 'Report which ui kinds are available.',
                    parameters: z.object({}),
                    execute(_args, ctx) {
                        for (const k of ['select', 'confirm', 'input', 'notify', 'set_status', 'set_widget', 'set_title'] as const) {
                            if (ctx.hasUI(k)) seen.push(k);
                        }
                        return { content: 'ok' };
                    },
                }),
            );
        });
        host = createTestHost(ext);
        await host.initialize({ mode: 'tui', ui_capabilities: ['confirm', 'set_widget'] });
        await host.callTool('probe', {});
        expect(seen).toEqual(['confirm', 'set_widget']);
    });

    it('reports no UI on a headless handshake (empty ui_capabilities)', async () => {
        let any = true;
        const ext = defineExtension((smooth) => {
            smooth.name = 'probe';
            smooth.version = '0.0.1';
            smooth.registerTool(
                defineTool({
                    name: 'probe',
                    description: 'Report UI availability.',
                    parameters: z.object({}),
                    execute(_args, ctx) {
                        any = ctx.hasUI('confirm') || ctx.hasUI('set_widget');
                        return { content: 'ok' };
                    },
                }),
            );
        });
        host = createTestHost(ext);
        await host.initialize(); // defaults to mode: 'headless', no ui_capabilities
        await host.callTool('probe', {});
        expect(any).toBe(false);
    });
});

describe('ctx.ui round-trips ui/request', () => {
    it('todo.clear asks the host to confirm and honors the answer', async () => {
        const asked: UiRequestParams[] = [];
        host = createTestHost(createTodo(), {
            onUiRequest: (params) => {
                asked.push(params);
                if (params.kind === 'confirm') return { confirmed: true };
                return {}; // set_widget etc.
            },
        });
        await host.initialize({ mode: 'tui', ui_capabilities: ['confirm', 'set_widget'] });
        await host.callTool('add', { text: 'ship phase 3' });
        const res = await host.callTool('clear', {});
        expect(res.content).toMatch(/Cleared 1 todos/);
        // The confirm was asked before clearing.
        expect(asked.some((p) => p.kind === 'confirm')).toBe(true);
    });

    it('a declined confirm cancels the clear', async () => {
        host = createTestHost(createTodo(), {
            onUiRequest: (params) => (params.kind === 'confirm' ? { confirmed: false } : {}),
        });
        await host.initialize({ mode: 'tui', ui_capabilities: ['confirm', 'set_widget'] });
        await host.callTool('add', { text: 'keep me' });
        const res = await host.callTool('clear', {});
        expect(res.content).toBe('Cancelled.');
    });

    it('todo.add pushes a set_widget render block with a text fallback', async () => {
        const widgets: Record<string, unknown>[] = [];
        host = createTestHost(createTodo(), {
            onUiRequest: (params) => {
                if (params.kind === 'set_widget') widgets.push(params.widget);
                return {};
            },
        });
        await host.initialize({ mode: 'tui', ui_capabilities: ['set_widget'] });
        await host.callTool('add', { text: 'render me' });
        expect(widgets).toHaveLength(1);
        expect(widgets[0]).toMatchObject({ kind: 'keyvalue', title: 'Todos' });
        expect(typeof widgets[0]!.text).toBe('string');
    });
});

describe('headless host', () => {
    it('todo degrades: no widget, no confirm, clear proceeds', async () => {
        host = createTestHost(createTodo()); // no onUiRequest → NoUI
        await host.initialize(); // headless
        await host.callTool('add', { text: 'a' });
        const res = await host.callTool('clear', {});
        expect(res.content).toMatch(/Cleared 1 todos/);
    });

    it('calling ctx.ui.* against a headless host rejects with -32001 NoUI', async () => {
        let caught: unknown;
        const ext = defineExtension((smooth) => {
            smooth.name = 'ungated';
            smooth.version = '0.0.1';
            smooth.registerTool(
                defineTool({
                    name: 'ask',
                    description: 'Ask without gating on hasUI.',
                    parameters: z.object({}),
                    async execute(_args, ctx) {
                        try {
                            await ctx.ui.confirm('proceed?');
                        } catch (err) {
                            caught = err;
                        }
                        return { content: 'done' };
                    },
                }),
            );
        });
        host = createTestHost(ext); // headless: ui/request → NoUI
        await host.initialize();
        await host.callTool('ask', {});
        expect(caught).toBeInstanceOf(RpcError);
        expect((caught as RpcError).code).toBe(errorCode.NoUI);
    });
});
