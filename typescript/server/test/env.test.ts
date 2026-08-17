/**
 * The env-parity contract: every server implementation reads the canonical
 * `SMOOTH_AGENT_*` names, and each one's pre-parity name keeps working as an alias
 * so no existing deployment breaks. Canonical wins when both are set.
 */
import { describe, expect, it } from 'vitest';

import { resolveBind, resolveModel } from '../src/env.js';

describe('resolveBind', () => {
    it('defaults to loopback:8787, matching the sibling hosts', () => {
        expect(resolveBind({})).toEqual({ host: '127.0.0.1', port: 8787 });
    });

    it('reads the canonical names', () => {
        expect(resolveBind({ SMOOTH_AGENT_BIND: '0.0.0.0', SMOOTH_AGENT_PORT: '9000' })).toEqual({ host: '0.0.0.0', port: 9000 });
    });

    it('still reads this host’s pre-parity aliases', () => {
        expect(resolveBind({ SMOOTH_OPERATOR_HOST: '0.0.0.0', SMOOTH_OPERATOR_PORT: '8793' })).toEqual({ host: '0.0.0.0', port: 8793 });
    });

    it('prefers the canonical name over the alias', () => {
        const env = { SMOOTH_AGENT_BIND: '0.0.0.0', SMOOTH_OPERATOR_HOST: '10.0.0.1', SMOOTH_AGENT_PORT: '9000', SMOOTH_OPERATOR_PORT: '8793' };
        expect(resolveBind(env)).toEqual({ host: '0.0.0.0', port: 9000 });
    });

    it('falls back to the default port rather than binding NaN', () => {
        expect(resolveBind({ SMOOTH_AGENT_PORT: 'not-a-port' }).port).toBe(8787);
    });

    it('treats a blank value as unset', () => {
        expect(resolveBind({ SMOOTH_AGENT_BIND: '  ', SMOOTH_OPERATOR_HOST: '10.0.0.1' }).host).toBe('10.0.0.1');
    });
});

describe('resolveModel', () => {
    it('defaults, reads the canonical name, and prefers it over the alias', () => {
        expect(resolveModel({})).toBe('claude-haiku-4-5');
        expect(resolveModel({ SMOOTH_AGENT_MODEL: 'a' })).toBe('a');
        expect(resolveModel({ SMOOAI_MODEL: 'b' })).toBe('b');
        expect(resolveModel({ SMOOTH_AGENT_MODEL: 'a', SMOOAI_MODEL: 'b' })).toBe('a');
    });
});
