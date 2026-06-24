"""Connection auth — resolving the ``?token=`` slot into an access context.

The Python analog of the C# ``Auth.cs`` (Principal / AccessContext / verifier seam)
and the Rust verifier seam (``NoAuthVerifier`` / ``LocalTokenVerifier``). Browsers
can't set custom headers on a WebSocket handshake, so the bearer token rides on the
query string and is resolved to an :class:`AccessContext` at connect time.

Fail closed: anything missing, malformed, expired, or failing verification resolves
to :data:`AccessContext.ANONYMOUS` (org-public) — never an all-access principal.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import time
from abc import ABC, abstractmethod
from dataclasses import dataclass, field


@dataclass(frozen=True)
class Principal:
    """An authenticated identity. Mirrors the engine's ``Principal``."""

    sub: str
    org: str
    role: str
    groups: list[str] = field(default_factory=list)


#: The org-public, unauthenticated principal.
ANONYMOUS_PRINCIPAL = Principal(sub="anonymous", org="public", role="anonymous", groups=[])


@dataclass(frozen=True)
class AccessContext:
    """The access context threaded through a turn — who's asking, for ACL-filtered
    retrieval. Mirrors the Rust/C# ``AccessContext``. Fails closed: absent/invalid
    identity is anonymous (org-public)."""

    principal: Principal
    is_anonymous: bool

    @property
    def groups(self) -> list[str]:
        return self.principal.groups


AccessContext.ANONYMOUS = AccessContext(principal=ANONYMOUS_PRINCIPAL, is_anonymous=True)  # type: ignore[attr-defined]


class AuthVerifier(ABC):
    """Resolves a connection token into an :class:`AccessContext`. The seam the
    server is wired with at connect time (mirrors the Rust ``AuthVerifier`` trait
    and the C# ``TokenAccessResolver``)."""

    @abstractmethod
    def resolve(self, token: str | None) -> AccessContext: ...

    @abstractmethod
    def mode(self) -> str:
        """A short label for logs (``none`` / ``local`` / ...)."""
        ...


class NoAuthVerifier(AuthVerifier):
    """The default permissive verifier: every connection is anonymous (org-public).
    Mirrors the Rust ``NoAuthVerifier`` — used by the local flavor and protocol-only
    paths."""

    def resolve(self, token: str | None) -> AccessContext:
        return AccessContext.ANONYMOUS  # type: ignore[attr-defined]

    def mode(self) -> str:
        return "none"


def _b64url_decode(value: str) -> bytes:
    """Decode a base64url string, tolerating missing padding."""
    padding = "=" * (-len(value) % 4)
    return base64.urlsafe_b64decode(value + padding)


def _access_from_claims(payload: dict) -> AccessContext:
    """Build an :class:`AccessContext` from a decoded claims dict, enforcing ``exp``.
    Raises on an expired token (the caller fails closed to anonymous)."""
    exp = payload.get("exp")
    if isinstance(exp, (int, float)) and exp < time.time():
        raise ValueError("token expired")
    groups = payload.get("groups")
    groups_list = [g for g in groups if isinstance(g, str)] if isinstance(groups, list) else []
    principal = Principal(
        sub=str(payload.get("sub", "unknown")),
        org=str(payload.get("org", "public")),
        role=str(payload.get("role", "basic")),
        groups=groups_list,
    )
    return AccessContext(principal=principal, is_anonymous=False)


class LocalTokenVerifier(AuthVerifier):
    """Resolves a token as an HS256-signed JWT (``header.payload.signature``),
    failing closed to anonymous on any error. Mirrors the Rust ``LocalTokenVerifier``
    and the C# ``TokenAccessResolver`` JWT path.

    The signature is verified in constant time against ``secret``; the ``exp`` claim
    (when present) is enforced. A missing/empty token, a malformed JWT, a bad
    signature, or an expired token all degrade to :data:`AccessContext.ANONYMOUS`."""

    def __init__(self, secret: str) -> None:
        if not secret:
            raise ValueError("LocalTokenVerifier requires a non-empty HS256 secret")
        self._secret = secret.encode("utf-8")

    def resolve(self, token: str | None) -> AccessContext:
        if not token:
            return AccessContext.ANONYMOUS  # type: ignore[attr-defined]
        try:
            parts = token.split(".")
            if len(parts) != 3:
                raise ValueError("malformed JWT")
            signing_input = f"{parts[0]}.{parts[1]}".encode("ascii")
            expected = hmac.new(self._secret, signing_input, hashlib.sha256).digest()
            actual = _b64url_decode(parts[2])
            if not hmac.compare_digest(expected, actual):
                raise ValueError("bad signature")
            payload = json.loads(_b64url_decode(parts[1]))
            return _access_from_claims(payload)
        except Exception:
            # Fail closed: malformed / bad signature / expired → anonymous.
            return AccessContext.ANONYMOUS  # type: ignore[attr-defined]

    def mode(self) -> str:
        return "local"
