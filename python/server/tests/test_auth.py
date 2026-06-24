"""Auth verifier seam — the permissive default and the local HS256-JWT verifier,
both fail-closed to anonymous."""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import time

from smooth_operator_server.auth import (
    AccessContext,
    LocalTokenVerifier,
    NoAuthVerifier,
)


def _b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def _make_jwt(secret: str, claims: dict) -> str:
    header = _b64url(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    payload = _b64url(json.dumps(claims).encode())
    signing_input = f"{header}.{payload}".encode("ascii")
    sig = _b64url(hmac.new(secret.encode(), signing_input, hashlib.sha256).digest())
    return f"{header}.{payload}.{sig}"


def test_no_auth_is_always_anonymous() -> None:
    verifier = NoAuthVerifier()
    assert verifier.mode() == "none"
    ctx = verifier.resolve("anything")
    assert ctx.is_anonymous
    assert ctx is AccessContext.ANONYMOUS


def test_local_verifier_accepts_valid_token() -> None:
    secret = "topsecret"
    verifier = LocalTokenVerifier(secret)
    token = _make_jwt(
        secret,
        {"sub": "user-1", "org": "acme", "role": "admin", "groups": ["g1", "g2"], "exp": time.time() + 60},
    )
    ctx = verifier.resolve(token)
    assert not ctx.is_anonymous
    assert ctx.principal.sub == "user-1"
    assert ctx.principal.org == "acme"
    assert ctx.principal.role == "admin"
    assert ctx.groups == ["g1", "g2"]


def test_local_verifier_fails_closed_on_bad_signature() -> None:
    verifier = LocalTokenVerifier("topsecret")
    forged = _make_jwt("wrongsecret", {"sub": "attacker", "exp": time.time() + 60})
    assert verifier.resolve(forged).is_anonymous


def test_local_verifier_fails_closed_on_expired() -> None:
    secret = "topsecret"
    verifier = LocalTokenVerifier(secret)
    expired = _make_jwt(secret, {"sub": "user-1", "exp": time.time() - 1})
    assert verifier.resolve(expired).is_anonymous


def test_local_verifier_fails_closed_on_garbage_and_missing() -> None:
    verifier = LocalTokenVerifier("topsecret")
    assert verifier.resolve("not-a-jwt").is_anonymous
    assert verifier.resolve(None).is_anonymous
    assert verifier.resolve("").is_anonymous


def test_resolve_access_reads_token_from_ws_path() -> None:
    """The server resolves the connection's access from the ``?token=`` query slot
    of the WS request path — the integration point browsers rely on."""
    from smooth_operator_server.server import ServerState, _resolve_access
    from smooth_operator_server.session_store import InMemorySessionStore

    secret = "sek"
    state = ServerState(store=InMemorySessionStore(), auth=LocalTokenVerifier(secret))
    token = _make_jwt(secret, {"sub": "u1", "org": "acme", "groups": ["g1"], "exp": time.time() + 60})

    ctx = _resolve_access(state, f"/ws?token={token}")
    assert not ctx.is_anonymous
    assert ctx.principal.sub == "u1"
    assert ctx.principal.org == "acme"

    # No token on the path → anonymous (fail closed).
    assert _resolve_access(state, "/ws").is_anonymous
