import { describe, expect, it } from 'vitest';
import { createTestHost, defineExtension } from '../src/index.js';
import { permissionGate } from '../examples/permission-gate.js';

describe('hook handlers', () => {
    it('permission-gate blocks a dangerous bash command via tool_call', async () => {
        const host = createTestHost(permissionGate);
        await host.initialize();

        const blocked = await host.callHook('tool_call', { tool: 'bash', arguments: { command: 'rm -rf /' } });
        expect(blocked.action).toBe('block');

        const ok = await host.callHook('tool_call', { tool: 'bash', arguments: { command: 'ls -la' } });
        expect(ok.action).toBe('continue');
        host.close();
    });

    it('a patch outcome shallow-merges onto the input', async () => {
        const ext = defineExtension((smooth) => {
            smooth.name = 'redactor';
            smooth.on('tool_result', () => ({ patch: { content: '[redacted]' } }));
        });
        const host = createTestHost(ext);
        await host.initialize();

        const outcome = await host.callHook('tool_result', { tool: 'bash', content: 'secret', is_error: false });
        expect(outcome).toEqual({ action: 'modify', patch: { tool: 'bash', content: '[redacted]', is_error: false } });
        host.close();
    });

    it('first block short-circuits later handlers', async () => {
        const ext = defineExtension((smooth) => {
            smooth.name = 'multi';
            smooth.on('tool_call', () => ({ block: true, reason: 'first' }));
            smooth.on('tool_call', () => ({ patch: { should: 'not apply' } }));
        });
        const host = createTestHost(ext);
        await host.initialize();
        expect(await host.callHook('tool_call', { tool: 'bash' })).toEqual({ action: 'block', reason: 'first' });
        host.close();
    });

    it('hook names are not reported as event subscriptions', async () => {
        const ext = defineExtension((smooth) => {
            smooth.name = 'mixed';
            smooth.on('tool_call', () => undefined);
            smooth.on('turn_start', () => undefined);
        });
        const host = createTestHost(ext);
        const init = await host.initialize();
        expect(init.registrations?.subscriptions).toEqual(['turn_start']);
        host.close();
    });
});
