import { describe, expect, it } from 'vitest';
import { createTestHost } from '@smooai/smooth-extension-sdk';
import { extension } from '../src/index.js';

describe('__NAME__ tool', () => {
    it('registers greet at handshake', async () => {
        const host = createTestHost(extension);
        const result = await host.initialize();
        expect(result.registrations?.tools?.map((t) => t.name)).toContain('greet');
        host.close();
    });

    it('executes greet and streams progress', async () => {
        const host = createTestHost(extension);
        await host.initialize();
        const updates: unknown[] = [];
        const res = await host.callTool('greet', { name: 'Ada' }, { onUpdate: (u) => updates.push(u) });
        expect(res).toEqual({ content: 'Hello, Ada!' });
        expect(updates[0]).toMatchObject({ message: 'greeting Ada', progress: 0.5 });
        host.close();
    });
});
