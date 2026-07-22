/**
 * The auth verifier seam: resolve a connection token into an {@link AccessContext}.
 *
 * The TypeScript port of the C# `Auth.cs` `TokenAccessResolver` and the Rust
 * server's verifier seam (`none` / `trusted` / `jwt`, plus a local-token verifier).
 *
 * Browsers can't set WebSocket request headers, so the identity rides in the
 * `?token=` query slot. Resolution is **fail-closed**: anything missing, malformed,
 * expired, or failing signature verification resolves to anonymous (org-public) —
 * never to an all-access principal.
 */
import { createHmac, timingSafeEqual } from 'node:crypto';

/** An authenticated identity. Mirrors the engine's `Principal`. */
export interface Principal {
    sub: string;
    org: string;
    role: string;
    groups: string[];
    /**
     * The authenticated user's email, from the `email` claim. This is the ONLY
     * trusted source of the caller's identity for per-user conversation scoping —
     * the client-supplied `userEmail` frame field is attacker-controlled and must
     * never decide what a caller may read. Absent → the connection has no scopable
     * identity and conversation reads fail closed (empty list, reads denied).
     */
    email?: string;
}

export const ANONYMOUS_PRINCIPAL: Principal = {
    sub: 'anonymous',
    org: 'public',
    role: 'anonymous',
    groups: [],
};

/**
 * The access context threaded through a turn — who's asking, for ACL-filtered
 * retrieval. Mirrors the Rust/C# `AccessContext`.
 */
export interface AccessContext {
    principal: Principal;
    isAnonymous: boolean;
    /**
     * Whether the server has auth CONFIGURED at all (verifier mode !== `none`), as
     * opposed to whether this particular connection authenticated. The two anonymous
     * outcomes need different handling for conversation scoping: auth-off means
     * single-tenant local/dev, where conversation reads stay unscoped; auth-on with a
     * bad/absent token means a real multi-tenant server, where reads must fail closed
     * rather than fall back to everyone's data.
     */
    authEnabled: boolean;
}

export const ANONYMOUS_ACCESS: AccessContext = {
    principal: ANONYMOUS_PRINCIPAL,
    isAnonymous: true,
    authEnabled: false,
};

/**
 * Anonymous on a server that DOES have auth configured — the fail-closed landing spot
 * for a missing/malformed/expired token. Distinct from {@link ANONYMOUS_ACCESS} only in
 * `authEnabled`, which keeps conversation reads scoped (and therefore empty/denied)
 * instead of unscoped.
 */
const ANONYMOUS_UNDER_AUTH: AccessContext = {
    principal: ANONYMOUS_PRINCIPAL,
    isAnonymous: true,
    authEnabled: true,
};

/** The verifier seam. A connection's `?token=` is resolved by one of these. */
export interface AuthVerifier {
    /** The mode label, mirroring the Rust verifier's `mode()` (`none` / `trusted` / `jwt`). */
    readonly mode: string;
    /** Resolve a (possibly absent) token into an access context. Must fail closed. */
    resolve(token: string | undefined): AccessContext;
}

/**
 * No auth — every connection is anonymous (org-public). The default, mirroring the
 * Rust `NoAuthVerifier` and C# `AuthMode.None`. Used by {@link serveLocal}.
 */
export class NoAuthVerifier implements AuthVerifier {
    readonly mode = 'none';
    resolve(_token: string | undefined): AccessContext {
        return ANONYMOUS_ACCESS;
    }
}

/**
 * The token is `base64url(JSON)` identity forwarded by a TRUSTED proxy — decoded,
 * not cryptographically verified. Mirrors the Rust/C# `trusted` mode. Use only
 * behind a proxy that authenticates the user and mints the claims.
 */
export class TrustedTokenVerifier implements AuthVerifier {
    readonly mode = 'trusted';
    resolve(token: string | undefined): AccessContext {
        if (!token) return ANONYMOUS_UNDER_AUTH;
        try {
            const json = base64UrlDecode(token).toString('utf8');
            return fromClaims(JSON.parse(json));
        } catch {
            return ANONYMOUS_UNDER_AUTH;
        }
    }
}

/**
 * The token is an HS256 JWT; the signature is verified against a shared secret and
 * `exp` is enforced. Mirrors the Rust/C# `jwt` mode (the local-token verifier).
 * Any verification failure fails closed to anonymous.
 */
export class LocalTokenVerifier implements AuthVerifier {
    readonly mode = 'jwt';
    private readonly secret: Buffer;

    constructor(hs256Secret: string) {
        if (!hs256Secret) throw new Error('LocalTokenVerifier requires a non-empty HS256 secret');
        this.secret = Buffer.from(hs256Secret, 'utf8');
    }

    resolve(token: string | undefined): AccessContext {
        if (!token) return ANONYMOUS_UNDER_AUTH;
        try {
            const parts = token.split('.');
            if (parts.length !== 3) throw new Error('malformed JWT');
            const [header, payload, signature] = parts as [string, string, string];

            const expected = createHmac('sha256', this.secret).update(`${header}.${payload}`).digest();
            const actual = base64UrlDecode(signature);
            // Length-check first: timingSafeEqual throws on a length mismatch.
            if (expected.length !== actual.length || !timingSafeEqual(expected, actual)) {
                throw new Error('bad signature');
            }

            const claims = JSON.parse(base64UrlDecode(payload).toString('utf8'));
            return fromClaims(claims);
        } catch {
            return ANONYMOUS_UNDER_AUTH;
        }
    }
}

/** Map raw JWT/identity claims to an authenticated {@link AccessContext}. */
function fromClaims(claims: Record<string, unknown>): AccessContext {
    const exp = claims.exp;
    if (typeof exp === 'number' && exp * 1000 < Date.now()) {
        throw new Error('token expired');
    }

    const groups = Array.isArray(claims.groups) ? claims.groups.filter((g): g is string => typeof g === 'string') : [];

    const principal: Principal = {
        sub: typeof claims.sub === 'string' ? claims.sub : 'unknown',
        org: typeof claims.org === 'string' ? claims.org : 'public',
        role: typeof claims.role === 'string' ? claims.role : 'basic',
        groups,
        // The scoping identity. A token without an `email` claim yields a principal
        // with no email — conversation reads then fail closed rather than unscoped.
        ...(typeof claims.email === 'string' && claims.email.length > 0 ? { email: claims.email } : {}),
    };
    return { principal, isAnonymous: false, authEnabled: true };
}

/** Decode a base64url string (no padding) to bytes. */
function base64UrlDecode(value: string): Buffer {
    return Buffer.from(value, 'base64url');
}
