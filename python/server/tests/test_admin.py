"""The ``/admin/*`` API the console drives.

Two things matter per route: it must fail CLOSED without a sufficient token, and
it must answer the wire shape the console's typed client expects (camelCase,
``{"error":{"code","message"}}``).

Driven over REAL HTTP against a booted server, so the ``process_request`` hook,
the auth gate and the JSON all have to work together.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any
from urllib.error import HTTPError
from urllib.request import Request, urlopen

import pytest

from smooth_operator_server.auth import AccessContext, AuthVerifier, Principal
from smooth_operator_server.server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore

# Every gated route with the minimum role it requires — the contract table.
GATED_ROUTES = [
    ("GET", "/admin/me"),
    ("GET", "/admin/conversations"),
    ("GET", "/admin/conversations/c1/messages"),
    ("GET", "/admin/indexing/runs"),
    ("GET", "/admin/document-sets"),
    ("GET", "/admin/connectors"),
    ("POST", "/admin/connectors"),
    ("GET", "/admin/connectors/x"),
    ("PUT", "/admin/connectors/x"),
    ("DELETE", "/admin/connectors/x"),
    ("POST", "/admin/connectors/x/index"),
    ("GET", "/admin/settings"),
    ("PUT", "/admin/settings"),
]


class RoleVerifier(AuthVerifier):
    """An auth-enabled verifier that maps a token straight to a role."""

    def resolve(self, token: str | None) -> AccessContext:
        if token in ("admin", "curator", "basic"):
            return AccessContext(principal=Principal(sub=f"u-{token}", org="org-1", role=token), is_anonymous=False)
        if token == "other-org-admin":
            return AccessContext(principal=Principal(sub="u-other", org="org-2", role="admin"), is_anonymous=False)
        return AccessContext(principal=Principal(sub="anonymous", org="public", role="anonymous"), is_anonymous=True)

    def mode(self) -> str:
        return "test"


async def _boot(verifier: AuthVerifier | None) -> Any:
    state = ServerState(store=InMemorySessionStore(), **({"auth": verifier} if verifier else {}))
    return await serve(state, "127.0.0.1", 0)


async def call(port: int, method: str, path: str, token: str | None = None, body: Any = None):
    """Issue an admin request, returning (status, parsed-json-or-None).

    urlopen blocks, and the server shares this test's event loop — calling it
    inline deadlocks (the server can never answer). Run it off-loop.
    """
    return await asyncio.to_thread(_call_sync, port, method, path, token, body)


def _call_sync(port: int, method: str, path: str, token: str | None = None, body: Any = None):
    data = None if body is None or method in ("GET", "HEAD") else json.dumps(body).encode()
    req = Request(f"http://127.0.0.1:{port}{path}", data=data, method=method)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urlopen(req) as resp:
            raw = resp.read().decode()
            return resp.status, (json.loads(raw) if raw else None)
    except HTTPError as e:
        raw = e.read().decode()
        return e.code, (json.loads(raw) if raw else None)


@pytest.fixture
async def authed():
    server = await _boot(RoleVerifier())
    yield server.admin_port
    await server.shutdown()


@pytest.fixture
async def dev_mode():
    # No verifier passed → the default NoAuthVerifier, mode "none".
    server = await _boot(None)
    yield server.admin_port
    await server.shutdown()


# ---- auth gate ----


async def test_fails_closed_on_every_gated_route(authed) -> None:
    for method, path in GATED_ROUTES:
        status, payload = await call(authed, method, path, body={})
        assert status == 401, f"{method} {path}"
        assert payload["error"]["code"] == "UNAUTHENTICATED", f"{method} {path}"


async def test_rejects_an_invalid_token(authed) -> None:
    status, payload = await call(authed, "GET", "/admin/me", "garbage")
    assert status == 401
    assert payload["error"]["code"] == "INVALID_TOKEN"


async def test_enforces_role_rank_in_both_directions(authed) -> None:
    assert (await call(authed, "GET", "/admin/me", "basic"))[0] == 200
    assert (await call(authed, "GET", "/admin/settings", "basic"))[0] == 403
    assert (await call(authed, "GET", "/admin/settings", "curator"))[0] == 200

    status, payload = await call(authed, "PUT", "/admin/settings", "curator", {"model": "m"})
    assert status == 403
    assert payload["error"]["code"] == "FORBIDDEN"


async def test_health_is_ungated(authed) -> None:
    assert (await call(authed, "GET", "/admin/health"))[0] == 200


# ---- AUTH_MODE=none dev grant ----


async def test_no_auth_dev_mode_grants_admin(dev_mode) -> None:
    # Rust's NoAuthVerifier returns a fixed Admin principal there. Without the same
    # grant the console 403-walls against a local server — as useless as the 404s.
    for path in ("/admin/settings", "/admin/connectors", "/admin/indexing/runs"):
        assert (await call(dev_mode, "GET", path, "dev-token"))[0] == 200, path


async def test_no_auth_dev_mode_still_fails_closed_without_a_token(dev_mode) -> None:
    assert (await call(dev_mode, "GET", "/admin/settings"))[0] == 401


async def test_dev_grant_does_not_leak_into_an_auth_enabled_server(authed) -> None:
    assert (await call(authed, "GET", "/admin/settings", "basic"))[0] == 403


# ---- shapes the console consumes ----


async def test_me_returns_the_principal(authed) -> None:
    _, payload = await call(authed, "GET", "/admin/me", "curator")
    assert payload == {"userId": "u-curator", "orgId": "org-1", "role": "curator"}


async def test_conversations_and_messages_carry_their_envelopes(authed) -> None:
    _, listing = await call(authed, "GET", "/admin/conversations", "curator")
    assert isinstance(listing["conversations"], list)
    assert "nextCursor" in listing

    _, msgs = await call(authed, "GET", "/admin/conversations/c1/messages", "curator")
    assert msgs["conversationId"] == "c1"
    assert isinstance(msgs["messages"], list)


async def test_document_sets_is_an_empty_list(authed) -> None:
    _, payload = await call(authed, "GET", "/admin/document-sets", "curator")
    assert payload["documentSets"] == []


# ---- connector CRUD ----


async def test_connector_crud_round_trip(authed) -> None:
    status, created = await call(
        authed,
        "POST",
        "/admin/connectors",
        "admin",
        {"name": "docs", "kind": "web", "config": {"url": "https://x"}, "enabled": True},
    )
    assert status == 200
    connector = created["connector"]
    connector_id = connector["id"]
    assert connector_id
    assert connector["name"] == "docs" and connector["enabled"] is True
    assert connector["createdAt"] and connector["updatedAt"]
    # The internal owner key must never reach the wire.
    assert "_orgId" not in connector

    _, listing = await call(authed, "GET", "/admin/connectors", "curator")
    assert len(listing["connectors"]) == 1

    _, got = await call(authed, "GET", f"/admin/connectors/{connector_id}", "curator")
    assert got["connector"]["id"] == connector_id

    _, updated = await call(
        authed,
        "PUT",
        f"/admin/connectors/{connector_id}",
        "admin",
        {"name": "docs2", "kind": "web", "config": {}, "enabled": False},
    )
    assert updated["connector"]["name"] == "docs2" and updated["connector"]["enabled"] is False

    assert (await call(authed, "POST", f"/admin/connectors/{connector_id}/index", "curator"))[0] == 200
    _, runs = await call(authed, "GET", "/admin/indexing/runs", "curator")
    assert len(runs["runs"]) == 1

    assert (await call(authed, "DELETE", f"/admin/connectors/{connector_id}", "admin"))[0] == 204
    assert (await call(authed, "GET", f"/admin/connectors/{connector_id}", "curator"))[0] == 404


async def test_connectors_are_org_isolated(authed) -> None:
    _, created = await call(
        authed,
        "POST",
        "/admin/connectors",
        "admin",
        {"name": "mine", "kind": "web", "config": {}, "enabled": True},
    )
    connector_id = created["connector"]["id"]

    # A foreign id must be indistinguishable from an unknown one.
    assert (await call(authed, "GET", f"/admin/connectors/{connector_id}", "other-org-admin"))[0] == 404
    _, listing = await call(authed, "GET", "/admin/connectors", "other-org-admin")
    assert listing["connectors"] == []


async def test_connector_create_validates(authed) -> None:
    assert (await call(authed, "POST", "/admin/connectors", "admin", {"kind": "web"}))[0] == 400


# ---- settings ----


async def test_settings_defaults_then_round_trip(authed) -> None:
    _, initial = await call(authed, "GET", "/admin/settings", "curator")
    assert initial["settings"]["orgId"] == "org-1"
    assert initial["settings"]["model"]

    _, put = await call(
        authed,
        "PUT",
        "/admin/settings",
        "admin",
        {"model": "claude-sonnet-4-5", "systemPrompt": "be nice", "defaultTools": ["search"]},
    )
    assert put["settings"]["model"] == "claude-sonnet-4-5"
    assert put["settings"]["systemPrompt"] == "be nice"

    _, reread = await call(authed, "GET", "/admin/settings", "curator")
    assert reread["settings"]["model"] == "claude-sonnet-4-5"


async def test_settings_write_requires_a_model(authed) -> None:
    assert (await call(authed, "PUT", "/admin/settings", "admin", {"systemPrompt": "x"}))[0] == 400
