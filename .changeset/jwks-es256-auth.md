---
"@smooai/smooth-operator": minor
---

feat(auth): JWKS-based JWT verification (ES256 + any algorithm, with rotation) for `smoo`/`jwt` modes

The auth verifier could only validate tokens against a **static RS256 PEM**
(`AUTH_JWT_RS256_PUBLIC_KEY`). SmooAI's `auth.smoo.ai` (the `smoo` issuer) signs
dashboard tokens with **ES256** (`/.well-known/jwks.json` → `alg: ES256, kty: EC`),
so every real SmooAI token was rejected — blocking `AUTH_MODE=smoo` for the SmooAI
K8s flavor.

This adds a JWKS-backed verification path (additive, behavior-preserving):

- New optional `AUTH_JWT_JWKS_URL`, and auto-derivation of
  `{AUTH_JWT_ISSUER}/.well-known/jwks.json` when an issuer is set and no static
  key is given.
- Keys are fetched, **cached** (TTL) and **rotation-aware** (refresh-on-unknown-`kid`),
  selected per-token by `kid`, and validated with the key's algorithm via
  `DecodingKey::from_jwk` — so **any** advertised JWS algorithm works
  (ES256/ES384/RS256/PS256/EdDSA/…), not just RS256.
- Wired into both `SmooIdentityVerifier` (the `smoo` path) and `JwtVerifier`
  (BYO), so any OIDC issuer works. `AuthVerifier::verify` stays **synchronous**
  (the keyset is read from cache; the network fetch is off the hot path).

Key-source precedence (`jwt`/`smoo`): static `AUTH_JWT_RS256_PUBLIC_KEY` →
static `AUTH_JWT_HS256_SECRET` → JWKS (`AUTH_JWT_JWKS_URL`, else issuer-derived).
The static-RS256/HS256 paths are unchanged. With this, `AUTH_MODE=smoo` needs
only `AUTH_JWT_ISSUER` (+ optional audience) — no static public key.
