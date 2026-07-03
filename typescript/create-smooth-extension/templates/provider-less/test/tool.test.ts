import { describe, expect, it } from 'vitest';
import { createTestHost } from '@smooai/smooth-extension-sdk';
import { extension } from '../src/index.js';

describe('__NAME__ provider-less tool', () => {
    it('registers shout at handshake', async () => {
        const host = createTestHost(extension);
        const result = await host.initialize();
        expect(result.registrations?.tools?.map((t) => t.name)).toContain('shout');
        host.close();
    });

    it('uppercases its input with no external provider', async () => {
        const host = createTestHost(extension);
        await host.initialize();
        const res = await host.callTool('shout', { text: 'hello' });
        expect(res).toEqual({ content: 'HELLO' });
        host.close();
    });
});
