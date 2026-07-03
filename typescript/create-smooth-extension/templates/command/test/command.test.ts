import { describe, expect, it } from 'vitest';
import { createTestHost } from '@smooai/smooth-extension-sdk';
import { extension } from '../src/index.js';

describe('__NAME__ command', () => {
    it('registers /echo at handshake', async () => {
        const host = createTestHost(extension);
        const result = await host.initialize();
        expect(result.registrations?.commands?.map((c) => c.name)).toContain('echo');
        host.close();
    });

    it('executes /echo with an argument', async () => {
        const host = createTestHost(extension);
        await host.initialize();
        const res = await host.runCommand('echo', { phrase: 'hi there' });
        expect(res).toEqual({ content: 'You said: hi there' });
        host.close();
    });

    it('offers argument completions', async () => {
        const host = createTestHost(extension);
        await host.initialize();
        const res = await host.completeCommand('echo', 'he');
        expect(res.completions).toEqual([{ value: 'hello' }]);
        host.close();
    });
});
