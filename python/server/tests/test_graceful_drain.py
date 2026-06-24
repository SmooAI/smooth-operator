"""Graceful-drain test: a cancel that fires while a turn is in flight lets the turn
finish, then the connection loop exits and the backplane detach runs
(detach-after-loop).

Drives :func:`smooth_operator_server.server._connection_loop` directly with a fake
in-memory websocket so frame delivery + the cancel timing are deterministic — no
real socket needed.
"""

from __future__ import annotations

import asyncio
import json

from smooth_operator_core import MockLlmProvider

from smooth_operator_server.auth import AccessContext
from smooth_operator_server.backplane import InMemoryBackplane
from smooth_operator_server.server import ServerState, _connection_loop
from smooth_operator_server.session_store import InMemorySessionStore


class FakeWebSocket:
    """An in-memory duplex stand-in for a ``websockets`` connection. ``recv`` pulls
    from a queue the test feeds; ``send`` records outbound frames."""

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


async def test_in_flight_turn_finishes_then_loop_exits_and_detaches() -> None:
    """Set the cancel mid-turn → the in-flight turn still completes (we see its
    eventual_response) → the loop exits → detach runs (backplane empty)."""
    mock = MockLlmProvider()
    mock.push_text("a complete answer")
    store = InMemorySessionStore()
    backplane = InMemoryBackplane()
    state = ServerState(store=store, chat_client=mock, backplane=backplane)

    # Pre-create a session so the turn can run.
    session = await store.create_session("agent", "Alice", None)

    ws = FakeWebSocket()
    # Queue exactly one send_message; the loop will dispatch it.
    ws.feed(
        {
            "action": "send_message",
            "requestId": "r-drain",
            "sessionId": session.session_id,
            "message": "please answer",
        }
    )

    loop_task = asyncio.create_task(_connection_loop(ws, state, AccessContext.ANONYMOUS))

    # Let the loop pick up the frame and start the turn, then fire the cancel
    # mid-flight. Because dispatch is awaited inside the frame branch, the turn
    # must finish (emit eventual_response) before the loop checks the cancel again.
    await asyncio.sleep(0.05)
    state.cancel.set()

    # The loop must exit on its own (no more frames; cancel set).
    await asyncio.wait_for(loop_task, timeout=5.0)

    types = [e["type"] for e in ws.sent]
    assert "eventual_response" in types, f"in-flight turn must finish; saw {types}"
    eventual = next(e for e in ws.sent if e["type"] == "eventual_response")
    assert eventual["data"]["data"]["response"]["responseParts"] == ["a complete answer"]

    # detach-after-loop: the connection was deregistered from the backplane.
    assert backplane.attached_count == 0


async def test_cancel_before_any_frame_exits_without_blocking() -> None:
    """A cancel that arrives with no frame in flight stops the loop promptly and
    still detaches."""
    store = InMemorySessionStore()
    backplane = InMemoryBackplane()
    state = ServerState(store=store, chat_client=None, backplane=backplane)
    state.cancel.set()  # already draining before the loop even starts

    ws = FakeWebSocket()  # never fed a frame
    await asyncio.wait_for(_connection_loop(ws, state, AccessContext.ANONYMOUS), timeout=5.0)
    assert backplane.attached_count == 0
