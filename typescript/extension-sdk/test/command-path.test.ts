/**
 * The Phase 4 surface, SDK-side: command registration + dispatch + autocomplete,
 * flag delivery, shortcut declarations, and the session-action tier guard —
 * driven through the `plan-mode` flagship demo, which exercises phases 2–4
 * together (flag + command + tool_call intercept + widget + appendEntry).
 */
import { afterEach, describe, expect, it } from 'vitest';
import { createTestHost, defineExtension, errorCode, type TestHost, type UiRequestParams } from '../src/index.js';
import { createPlanMode } from '../examples/plan-mode.js';

let host: TestHost | undefined;
afterEach(() => host?.close());

describe('registrations surface commands, flags, and shortcuts', () => {
    it('emits commands/flags/shortcuts from the handshake', async () => {
        host = createTestHost(createPlanMode());
        const result = await host.initialize({ mode: 'tui', ui_capabilities: ['set_widget'] });
        const regs = result.registrations!;
        expect(regs.commands).toEqual([{ name: 'plan', description: expect.any(String) }]);
        expect(regs.flags).toEqual(['plan']);
        expect(regs.shortcuts).toEqual([{ key: 'ctrl+p', command: 'plan', description: 'Toggle plan mode' }]);
    });
});

describe('command/execute + command/complete', () => {
    it('runs a registered command and returns its content', async () => {
        // Headless: no set_widget capability, so the command skips the widget.
        host = createTestHost(createPlanMode());
        await host.initialize({ mode: 'headless' });
        const out = await host.runCommand('plan', { state: 'on' });
        expect(out.content).toContain('enabled');
    });

    it('returns a friendly message for an unknown command', async () => {
        host = createTestHost(createPlanMode());
        await host.initialize();
        const out = await host.runCommand('nope');
        expect(out.content).toContain('unknown command');
    });

    it('round-trips argument autocomplete', async () => {
        host = createTestHost(createPlanMode());
        await host.initialize();
        const { completions } = await host.completeCommand('plan', 'o');
        expect(completions.map((c) => c.value)).toEqual(['on', 'off']);
        const none = await host.completeCommand('plan', 'zzz');
        expect(none.completions).toEqual([]);
    });
});

describe('session actions from a command handler', () => {
    it('appendEntry from a command reaches the host (command tier)', async () => {
        host = createTestHost(createPlanMode());
        await host.initialize({ mode: 'headless' });
        await host.runCommand('plan', { state: 'on' });
        const entries = host.sessionCalls.filter((c) => c.method === 'session/append_entry');
        expect(entries).toHaveLength(1);
        expect(entries[0]!.params.entry).toEqual({ kind: 'plan_mode', enabled: true });
        // The recorded context is command-tier — the guard let it through.
        expect((entries[0]!.params.context as { tier: string }).tier).toBe('command');
    });

    it('rejects a session action presented from an event-tier context (-32003)', async () => {
        // The test host enforces the same guard the engine does. A command whose
        // handler calls appendEntry, dispatched with an EVENT-tier context, has
        // its session call bounced with -32003 ContextViolation.
        const ext = defineExtension((smooth) => {
            smooth.name = 'probe';
            smooth.version = '0.0.1';
            smooth.registerCommand({
                name: 'leak',
                description: 'Try a session action with the wrong tier.',
                async execute(ctx) {
                    await ctx.session.appendEntry({ leaked: true });
                    return {};
                },
            });
        });
        host = createTestHost(ext);
        await host.initialize();
        await expect(host.runCommand('leak', {}, { token: 'epoch-1', tier: 'event' })).rejects.toMatchObject({
            code: errorCode.ContextViolation,
        });
    });
});

describe('plan-mode exercises phases 2–4 together', () => {
    /** A test host whose set_widget calls are captured. */
    const widgetHost = () => {
        const widgets: Record<string, unknown>[] = [];
        const onUiRequest = (p: UiRequestParams) => {
            if (p.kind === 'set_widget') widgets.push(p.widget);
            return {};
        };
        return { widgets, host: createTestHost(createPlanMode(), { onUiRequest }) };
    };

    it('the --plan flag blocks a write tool; toggling off unblocks it', async () => {
        const { host: h } = widgetHost();
        host = h;
        // Flag delivered at initialize → plan mode active.
        await h.initialize({ mode: 'tui', ui_capabilities: ['set_widget'], flags: { plan: true } });

        const blocked = await h.callHook('tool_call', { tool: 'write', arguments: { path: 'a.ts' } });
        expect(blocked.action).toBe('block');

        // A read tool is never blocked.
        const allowed = await h.callHook('tool_call', { tool: 'read', arguments: { path: 'a.ts' } });
        expect(allowed.action).toBe('continue');

        // Toggle off via the command → write is allowed again.
        await h.runCommand('plan', { state: 'off' });
        const nowAllowed = await h.callHook('tool_call', { tool: 'write', arguments: { path: 'a.ts' } });
        expect(nowAllowed.action).toBe('continue');
    });

    it('a toggle pushes a widget and persists an appendEntry', async () => {
        const { widgets, host: h } = widgetHost();
        host = h;
        await h.initialize({ mode: 'tui', ui_capabilities: ['set_widget'] });
        await h.runCommand('plan', { state: 'on' });
        expect(widgets).toHaveLength(1);
        expect(widgets[0]!.text).toContain('ON');
        expect(h.sessionCalls.some((c) => c.method === 'session/append_entry')).toBe(true);
    });

    it('a hot reload re-establishes plan mode from the re-delivered flag', async () => {
        // Simulate reload: a fresh extension process handshaken with the flag
        // still set restores plan mode without any prior toggle (the appendEntry
        // history lives on the host; the flag re-seeds the extension's state).
        host = createTestHost(createPlanMode());
        await host.initialize({ mode: 'headless', flags: { plan: true } });
        const stillBlocked = await host.callHook('tool_call', { tool: 'edit', arguments: {} });
        expect(stillBlocked.action).toBe('block');
    });
});
