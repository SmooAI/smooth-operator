/**
 * Auth verifier seam: the fail-closed token resolution mirrored from the Rust/C#
 * servers (none / trusted / jwt). Anything missing, malformed, expired, or with a
 * bad signature resolves to anonymous — never an all-access principal.
 */
import { createHmac } from 'node:crypto';
import { describe, expect, it } from 'vitest';

import { LocalTokenVerifier, NoAuthVerifier, TrustedTokenVerifier } from '../src/auth.js';

function b64url(input: Buffer | string): string {
    return Buffer.from(input).toString('base64url');
}

function trustedToken(claims: Record<string, unknown>): string {
    return b64url(JSON.stringify(claims));
}

function jwt(secret: string, claims: Record<string, unknown>): string {
    const header = b64url(JSON.stringify({ alg: 'HS256', typ: 'JWT' }));
    const payload = b64url(JSON.stringify(claims));
    const sig = createHmac('sha256', secret).update(`${header}.${payload}`).digest('base64url');
    return `${header}.${payload}.${sig}`;
}

describe('NoAuthVerifier', () => {
    it('always resolves anonymous', () => {
        const v = new NoAuthVerifier();
        expect(v.mode).toBe('none');
        expect(v.resolve(undefined).isAnonymous).toBe(true);
        expect(v.resolve('anything').isAnonymous).toBe(true);
    });
});

describe('TrustedTokenVerifier', () => {
    const v = new TrustedTokenVerifier();

    it('decodes base64url JSON claims into a principal', () => {
        const ctx = v.resolve(trustedToken({ sub: 'u1', org: 'acme', role: 'admin', groups: ['github:acme/private'] }));
        expect(ctx.isAnonymous).toBe(false);
        expect(ctx.principal.sub).toBe('u1');
        expect(ctx.principal.org).toBe('acme');
        expect(ctx.principal.groups).toEqual(['github:acme/private']);
    });

    it('fails closed on no token and on garbage', () => {
        expect(v.resolve(undefined).isAnonymous).toBe(true);
        expect(v.resolve('!!!not-base64-json!!!').isAnonymous).toBe(true);
    });

    it('fills sensible defaults for missing claims', () => {
        const ctx = v.resolve(trustedToken({ sub: 'only-sub' }));
        expect(ctx.principal.org).toBe('public');
        expect(ctx.principal.role).toBe('basic');
        expect(ctx.principal.groups).toEqual([]);
    });
});

describe('LocalTokenVerifier (HS256)', () => {
    const secret = 'test-secret';
    const v = new LocalTokenVerifier(secret);

    it('accepts a validly-signed, unexpired token', () => {
        const ctx = v.resolve(jwt(secret, { sub: 'u1', org: 'acme', groups: ['g1'], exp: Math.floor(Date.now() / 1000) + 3600 }));
        expect(ctx.isAnonymous).toBe(false);
        expect(ctx.principal.sub).toBe('u1');
        expect(ctx.principal.groups).toEqual(['g1']);
    });

    it('fails closed on a bad signature', () => {
        const forged = jwt('wrong-secret', { sub: 'attacker' });
        expect(v.resolve(forged).isAnonymous).toBe(true);
    });

    it('fails closed on an expired token', () => {
        const expired = jwt(secret, { sub: 'u1', exp: Math.floor(Date.now() / 1000) - 10 });
        expect(v.resolve(expired).isAnonymous).toBe(true);
    });

    it('fails closed on a malformed token', () => {
        expect(v.resolve('not.a.jwt.too.many.parts').isAnonymous).toBe(true);
        expect(v.resolve('onlyonepart').isAnonymous).toBe(true);
        expect(v.resolve(undefined).isAnonymous).toBe(true);
    });

    it('rejects construction with an empty secret', () => {
        expect(() => new LocalTokenVerifier('')).toThrow();
    });
});
