import { describe, expect, it } from 'vitest';
import { createTestHost } from '@smooai/smooth-extension-sdk';
import { extension } from '../src/index.js';

describe('__NAME__ permission gate', () => {
    it('blocks a dangerous bash command', async () => {
        const host = createTestHost(extension);
        await host.initialize();
        const outcome = await host.callHook('tool_call', { tool: 'bash', arguments: { command: 'rm -rf /' } });
        expect(outcome.action).toBe('block');
        host.close();
    });

    it('lets a safe command through', async () => {
        const host = createTestHost(extension);
        await host.initialize();
        const outcome = await host.callHook('tool_call', { tool: 'bash', arguments: { command: 'ls -la' } });
        expect(outcome.action).toBe('continue');
        host.close();
    });
});
