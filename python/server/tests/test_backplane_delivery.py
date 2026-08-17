"""Backplane fan-out + ``POST /admin/publish``.

The point of these tests is that a routed target ACTUALLY RECEIVES — a registry
that accepts an association but delivers nothing would pass a shape-only test
while being useless, and `delivered` would be a lying 0.

Ports the Rust reference's ``backplane.rs`` unit tests plus its
``tests/admin_publish.rs`` wire assertions.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any
from urllib.error import HTTPError
from urllib.request import Request, urlopen

import pytest

from smooth_operator_server.auth import AccessContext, AuthVerifier, Principal
from smooth_operator_server.backplane import InMemoryBackplane, Target
from smooth_operator_server.server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore

# ── the registry itself (ports rust/smooth-operator/src/backplane.rs tests) ──


def collector() -> tuple[Any, list[dict[str, Any]]]:
    """A sink that records what it was handed."""
    received: list[dict[str, Any]] = []
    return received.append, received


async def test_publishes_to_a_session_across_its_connections() -> None:
    bp = InMemoryBackplane()
    sink_a, got_a = collector()
    sink_b, got_b = collector()
    await bp.attach("conn-a", sink_a)
    await bp.attach("conn-b", sink_b)
    await bp.associate("conn-a", Target("session", "s1"))
    await bp.associate("conn-b", Target("session", "s1"))

    assert await bp.publish(Target("session", "s1"), {"hi": 1}) == 2
    assert got_a == [{"hi": 1}]
    assert got_b == [{"hi": 1}]


async def test_publishes_to_a_single_connection() -> None:
    bp = InMemoryBackplane()
    sink, got = collector()
    await bp.attach("conn-1", sink)

    assert await bp.publish(Target("connection", "conn-1"), {"ping": True}) == 1
    assert got == [{"ping": True}]


async def test_unknown_target_delivers_to_nobody() -> None:
    bp = InMemoryBackplane()
    assert await bp.publish(Target("session", "nope"), {"x": 1}) == 0


async def test_detach_removes_sink_and_associations() -> None:
    bp = InMemoryBackplane()
    sink, _ = collector()
    await bp.attach("conn-x", sink)
    await bp.associate("conn-x", Target("user", "u1"))
    assert bp.attached_count == 1

    await bp.detach("conn-x")
    assert bp.attached_count == 0
    assert await bp.publish(Target("user", "u1"), {"x": 1}) == 0
    assert await bp.publish(Target("connection", "conn-x"), {"x": 1}) == 0


async def test_a_connection_can_serve_multiple_targets() -> None:
    bp = InMemoryBackplane()
    sink, got = collector()
    await bp.attach("c", sink)
    await bp.associate("c", Target("session", "s"))
    await bp.associate("c", Target("org", "o"))

    assert await bp.publish(Target("org", "o"), {"e": "org"}) == 1
    assert await bp.publish(Target("session", "s"), {"e": "sess"}) == 1
    assert got == [{"e": "org"}, {"e": "sess"}]


async def test_reattach_replaces_the_sink() -> None:
    # A reconnect under the same id must not leave the dead socket receiving.
    bp = InMemoryBackplane()
    stale_sink, stale = collector()
    fresh_sink, fresh = collector()
    await bp.attach("c", stale_sink)
    await bp.attach("c", fresh_sink)

    assert await bp.publish(Target("connection", "c"), {"x": 1}) == 1
    assert stale == []
    assert fresh == [{"x": 1}]


# ── POST /admin/publish ─────────────────────────────────────────────────────


class RoleVerifier(AuthVerifier):
    def resolve(self, token: str | None) -> AccessContext:
        if token in ("admin", "curator", "basic"):
            return AccessContext(principal=Principal(sub=f"u-{token}", org="org-1", role=token), is_anonymous=False)
        return AccessContext(principal=Principal(sub="anonymous", org="public", role="anonymous"), is_anonymous=True)

    def mode(self) -> str:
        return "test"


def _call_sync(port: int, method: str, path: str, token: str | None = None, body: Any = None):
    data = None if body is None else json.dumps(body).encode()
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


async def call(port: int, method: str, path: str, token: str | None = None, body: Any = None):
    # urlopen blocks and the server shares this loop — run it off-loop or deadlock.
    return await asyncio.to_thread(_call_sync, port, method, path, token, body)


@pytest.fixture
async def booted():
    backplane = InMemoryBackplane()
    state = ServerState(store=InMemorySessionStore(), auth=RoleVerifier(), backplane=backplane)
    server = await serve(state, "127.0.0.1", 0)
    yield server.admin_port, backplane
    await server.shutdown()


async def test_publish_requires_admin(booted) -> None:
    port, _ = booted
    payload = {"target": {"type": "connection", "id": "c"}, "event": {}}
    assert (await call(port, "POST", "/admin/publish", None, payload))[0] == 401
    assert (await call(port, "POST", "/admin/publish", "curator", payload))[0] == 403


async def test_publish_delivers_to_every_target_kind(booted) -> None:
    port, backplane = booted
    sink, got = collector()
    await backplane.attach("c1", sink)
    for kind, ident in (("session", "s1"), ("user", "u1"), ("org", "o1"), ("agent", "a1")):
        await backplane.associate("c1", Target(kind, ident))

    # Every one of the five kinds routes — none of them 501s here.
    for kind, ident in (
        ("connection", "c1"),
        ("session", "s1"),
        ("user", "u1"),
        ("org", "o1"),
        ("agent", "a1"),
    ):
        status, body = await call(
            port, "POST", "/admin/publish", "admin", {"target": {"type": kind, "id": ident}, "event": {"kind": kind}}
        )
        assert status == 200, (kind, body)
        assert body == {"delivered": 1}, (kind, body)

    assert [e["kind"] for e in got] == ["connection", "session", "user", "org", "agent"]


async def test_publish_reports_zero_rather_than_lying(booted) -> None:
    port, _ = booted
    status, body = await call(
        port, "POST", "/admin/publish", "admin", {"target": {"type": "session", "id": "ghost"}, "event": {}}
    )
    assert status == 200
    assert body == {"delivered": 0}


async def test_publish_rejects_a_bad_body(booted) -> None:
    port, _ = booted
    for payload in (
        {"target": {"type": "connection"}, "event": {}},  # no id
        {"target": {"type": "wat", "id": "x"}, "event": {}},  # unknown kind
        {"event": {}},  # no target
        {"target": {"type": "connection", "id": "c"}},  # no event
    ):
        status, body = await call(port, "POST", "/admin/publish", "admin", payload)
        assert status == 400, (payload, body)
        assert body["error"]["code"] == "INVALID_BODY", (payload, body)


# ── the connection lifecycle actually associates ────────────────────────────
# The route above works on a hand-attached connection. These prove the WIRING:
# that a REAL connection registers its sink and its targets, so a publish aimed
# at a live client's session/user/org/agent genuinely reaches that socket. Without
# these the route could pass every test above and still reach nobody in production.


class FakeWebSocket:
    """In-memory duplex stand-in for a ``websockets`` connection (same shape the
    graceful-drain test uses)."""

    def __init__(self) -> None:
        self._inbound: asyncio.Queue[str] = asyncio.Queue()
        self.sent: list[dict] = []
        self.path = ""

    def feed(self, frame: dict) -> None:
        self._inbound.put_nowait(json.dumps(frame))

    async def recv(self) -> str:
        return await self._inbound.get()

    async def send(self, data: str) -> None:
        self.sent.append(json.loads(data))


async def test_a_live_connection_is_reachable_by_session_user_org_and_agent() -> None:
    from smooth_operator_core import MockLlmProvider

    from smooth_operator_server.server import _connection_loop

    mock = MockLlmProvider()
    mock.push_text("hi")
    store = InMemorySessionStore()
    backplane = InMemoryBackplane()
    state = ServerState(store=store, chat_client=mock, backplane=backplane)
    access = AccessContext(principal=Principal(sub="u-1", org="org-1", role="basic"), is_anonymous=False)

    ws = FakeWebSocket()
    # create_conversation_session is what teaches the connection its session + agent.
    ws.feed({"action": "create_conversation_session", "requestId": "r1", "agentId": "agent-7"})
    loop_task = asyncio.create_task(_connection_loop(ws, state, access))

    # Wait for the session response, then publish at each learned target.
    for _ in range(200):
        if ws.sent:
            break
        await asyncio.sleep(0.01)
    assert ws.sent, "no create_conversation_session response"
    session_id = ws.sent[0]["data"]["sessionId"]

    delivered = {
        kind: await backplane.publish(Target(kind, ident), {"kind": kind})
        for kind, ident in (
            ("session", session_id),
            ("user", "u-1"),
            ("org", "org-1"),
            ("agent", "agent-7"),
        )
    }
    assert delivered == {"session": 1, "user": 1, "org": 1, "agent": 1}, delivered

    # And the events land on the actual socket, not just a counter.
    for _ in range(200):
        if len([f for f in ws.sent if "kind" in f]) == 4:
            break
        await asyncio.sleep(0.01)
    assert sorted(f["kind"] for f in ws.sent if "kind" in f) == ["agent", "org", "session", "user"]

    state.cancel.set()
    await asyncio.wait_for(loop_task, timeout=5)
    # Detach-after-loop: the closed connection is no longer a delivery target.
    assert await backplane.publish(Target("session", session_id), {"x": 1}) == 0
