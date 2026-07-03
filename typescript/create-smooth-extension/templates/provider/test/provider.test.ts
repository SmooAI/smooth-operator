import { describe, expect, it } from 'vitest';
import { createTestHost } from '@smooai/smooth-extension-sdk';
import { extension } from '../src/index.js';

describe('__NAME__ provider', () => {
    it('registers the provider + model at handshake', async () => {
        const host = createTestHost(extension);
        const result = await host.initialize();
        const providers = result.registrations?.providers ?? [];
        expect(providers.map((p) => p.name)).toContain('__NAME__');
        expect(providers[0]?.models?.map((m) => m.id)).toContain('__NAME__-1');
        host.close();
    });

    it('answers provider/complete', async () => {
        const host = createTestHost(extension);
        await host.initialize();
        const res = await host.complete('__NAME__', '__NAME__-1', [{ role: 'user', content: 'hi' }]);
        expect(res.content).toContain('hi');
        host.close();
    });

    it('streams provider/delta chunks when asked', async () => {
        const host = createTestHost(extension);
        await host.initialize();
        const chunks: string[] = [];
        await host.complete('__NAME__', '__NAME__-1', [{ role: 'user', content: 'stream me' }], {
            stream: true,
            onDelta: (e) => {
                if (e.type === 'Delta') chunks.push(e.content);
            },
        });
        expect(chunks.join('')).toContain('stream me');
        host.close();
    });
});
