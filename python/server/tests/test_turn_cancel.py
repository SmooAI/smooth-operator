"""User-initiated turn cancellation — the ``cancel`` action ("Stop button").

The Python port of the Rust reference ``tests/turn_cancel.rs``. It proves, over a
real WebSocket against the real Python server:

  1. **Cancel mid-turn stops it.** A ``cancel`` frame while a turn is parked in a
     tool cancels the turn *task* — the in-flight ``await`` is abandoned
     (``asyncio.CancelledError`` fires inside the tool, which never reaches its
     post-await line) — and a terminal ``cancelled`` event is emitted. No
     ``eventual_response`` follows.
  2. **Cancel with no active turn is a silent no-op** (no event; connection stays
     live).
  3. **A normal turn still completes** with an ``eventual_response`` (cancellation
     wiring doesn't disturb the happy path).
  4. **Disconnect mid-turn also aborts the turn** (no client remains to receive its
     output).

Plus the single-active-turn rule the same PR specifies: a second ``send_message``
while a turn is in flight is rejected with ``TURN_IN_PROGRESS``.

Runs fully offline: a :class:`~smooth_operator_core.MockLlmProvider` scripts the turn
and a deterministic tool parks it on a long sleep, giving a stable in-flight window
to cancel in. No gateway key.
"""

from __future__ import annotations

import asyncio
import json

import websockets
from smooth_operator_core import FunctionTool, MockLlmProvider

from smooth_operator_server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore

SLOW_TOOL = "slow_probe"


class SlowToolProbe:
    """A tool that parks the turn: it records that it started, then sleeps far
    longer than any test. Cancelling the turn task raises ``CancelledError`` at that
    sleep, so ``finished`` never sets and ``cancelled`` does — the positive signal
    that the turn was abandoned mid-await rather than run to completion (the Python
    analogue of the Rust drop-guard)."""

    def __init__(self) -> None:
        self.started = asyncio.Event()
        self.finished = asyncio.Event()
        self.cancelled = asyncio.Event()

    def tool(self) -> FunctionTool:
        async def _fn(_args: dict) -> str:
            self.started.set()
            try:
                await asyncio.sleep(3600)
            except asyncio.CancelledError:
                self.cancelled.set()
                raise
            # Only reached if the turn was NOT cancelled.
            self.finished.set()
            return "done"

        return FunctionTool(
            name=SLOW_TOOL,
            description="parks the turn for cancellation tests",
            parameters={"type": "object", "properties": {}},
            func=_fn,
        )


def _slow_tool_mock() -> MockLlmProvider:
    """A mock that calls the slow tool (so the turn parks and never returns)."""
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", SLOW_TOOL, "{}")
    return mock


def _answer_mock(text: str) -> MockLlmProvider:
    """A mock that just answers (no tool) so a turn completes normally."""
    mock = MockLlmProvider()
    mock.push_text(text)
    return mock


async def _start(chat_client, tools: list | None = None):
    state = ServerState(store=InMemorySessionStore(), chat_client=chat_client, tools=tools or [])
    return await serve(state, "127.0.0.1", 0)


async def _create_session(ws) -> str:
    await ws.send(
        json.dumps(
            {
                "action": "create_conversation_session",
                "requestId": "r-create",
                "agentId": "11111111-1111-1111-1111-111111111111",
            }
        )
    )
    while True:
        event = json.loads(await ws.recv())
        if event.get("type") == "immediate_response":
            return event["data"]["sessionId"]


async def _recv_until(ws, want: str, seen: list, timeout: float = 5.0) -> dict:
    """Read events until one of type ``want`` arrives; collect the skipped ones."""

    async def _pump() -> dict:
        while True:
            event = json.loads(await ws.recv())
            if event.get("type") == want:
                return event
            seen.append(event)

    return await asyncio.wait_for(_pump(), timeout)


async def _recv_within(ws, timeout: float) -> dict | None:
    """Try to receive one event within ``timeout``; ``None`` if none arrives."""
    try:
        return json.loads(await asyncio.wait_for(ws.recv(), timeout))
    except (TimeoutError, asyncio.TimeoutError, websockets.ConnectionClosed):
        return None


async def _send_message(ws, request_id: str, session_id: str, message: str = "please do the slow thing") -> None:
    await ws.send(
        json.dumps({"action": "send_message", "requestId": request_id, "sessionId": session_id, "message": message})
    )


async def test_cancel_mid_turn_aborts_and_emits_cancelled() -> None:
    probe = SlowToolProbe()
    server = await _start(_slow_tool_mock(), [probe.tool()])
    try:
        async with websockets.connect(server.ws_url()) as ws:
            session_id = await _create_session(ws)
            await _send_message(ws, "turn-1", session_id)

            # Wait until the turn is genuinely in flight (parked in the tool's await).
            await asyncio.wait_for(probe.started.wait(), 5)
            assert not probe.finished.is_set(), "tool must not have finished yet"

            # Cancel it (reusing the turn's requestId, the correlation convention).
            await ws.send(json.dumps({"action": "cancel", "requestId": "turn-1"}))

            # A terminal `cancelled` event arrives, echoing the turn's requestId.
            # (Skip any ack/stream events in flight before the cancel landed.)
            seen: list = []
            cancelled = await _recv_until(ws, "cancelled", seen)
            assert cancelled["requestId"] == "turn-1", cancelled
            assert cancelled["status"] == 499, cancelled
            assert cancelled["data"]["requestId"] == "turn-1"
            assert cancelled["data"]["status"] == 499
            assert isinstance(cancelled["timestamp"], int)
            # No answer payload: a cancelled turn produced no assistant message.
            assert "messageId" not in cancelled["data"]
            assert "response" not in cancelled["data"]

            # The turn was cancelled mid-await: the tool's post-await line never ran.
            await asyncio.wait_for(probe.cancelled.wait(), 5)
            assert not probe.finished.is_set(), "cancelled turn's tool must never reach its post-await completion"

            # No further terminal event (no eventual_response) follows.
            after = await _recv_within(ws, 0.5)
            assert after is None, f"no event should follow the cancellation, got: {after}"

            # Connection is still alive and usable.
            await ws.send(json.dumps({"action": "ping", "requestId": "p1"}))
            pong = json.loads(await ws.recv())
            assert pong["type"] == "pong"
            assert pong["requestId"] == "p1"
    finally:
        await server.shutdown()


async def test_cancelled_turn_keeps_the_user_message_and_persists_no_reply() -> None:
    """Partial output is discarded: the user message (persisted at the start of the
    turn) stays; the assistant reply (persisted only at the end) never lands."""
    probe = SlowToolProbe()
    store = InMemorySessionStore()
    state = ServerState(store=store, chat_client=_slow_tool_mock(), tools=[probe.tool()])
    server = await serve(state, "127.0.0.1", 0)
    try:
        async with websockets.connect(server.ws_url()) as ws:
            session_id = await _create_session(ws)
            session = await store.get_session(session_id)
            assert session is not None
            await _send_message(ws, "turn-1", session_id, "remember this")
            await asyncio.wait_for(probe.started.wait(), 5)

            await ws.send(json.dumps({"action": "cancel", "requestId": "turn-1"}))
            await _recv_until(ws, "cancelled", [])
            await asyncio.wait_for(probe.cancelled.wait(), 5)

            messages = await store.list_messages(session.conversation_id, 50)
            texts = [m.text for m in messages]
            assert texts == ["remember this"], texts
    finally:
        await server.shutdown()


async def test_cancel_with_no_active_turn_is_a_noop() -> None:
    server = await _start(_answer_mock("hi"))
    try:
        async with websockets.connect(server.ws_url()) as ws:
            await _create_session(ws)

            # Cancel with nothing running: must emit nothing.
            await ws.send(json.dumps({"action": "cancel", "requestId": "nope"}))

            # The next event is the pong (the cancel produced no event of its own).
            await ws.send(json.dumps({"action": "ping", "requestId": "p1"}))
            event = json.loads(await ws.recv())
            assert event["type"] == "pong", f"cancel must not emit an event; got: {event}"
            assert event["requestId"] == "p1"
    finally:
        await server.shutdown()


async def test_normal_turn_still_completes() -> None:
    server = await _start(_answer_mock("All done here."))
    try:
        async with websockets.connect(server.ws_url()) as ws:
            session_id = await _create_session(ws)
            await _send_message(ws, "turn-ok", session_id, "hello")

            seen: list = []
            done = await _recv_until(ws, "eventual_response", seen, timeout=10)
            assert done["requestId"] == "turn-ok", done
            assert done["status"] == 200
            assert not any(e["type"] == "cancelled" for e in seen), "a normal turn must not emit a cancelled event"
    finally:
        await server.shutdown()


async def test_second_send_message_mid_turn_is_rejected() -> None:
    """One active turn per connection: a second ``send_message`` while one is in
    flight is rejected with ``TURN_IN_PROGRESS``, never run concurrently."""
    probe = SlowToolProbe()
    server = await _start(_slow_tool_mock(), [probe.tool()])
    try:
        async with websockets.connect(server.ws_url()) as ws:
            session_id = await _create_session(ws)
            await _send_message(ws, "turn-1", session_id)
            await asyncio.wait_for(probe.started.wait(), 5)

            await _send_message(ws, "turn-2", session_id, "and another")
            seen: list = []
            err = await _recv_until(ws, "error", seen)
            assert err["error"]["code"] == "TURN_IN_PROGRESS", err
            assert err["requestId"] == "turn-2"

            # The first turn is untouched (still parked, not cancelled).
            assert not probe.cancelled.is_set()
            assert not probe.finished.is_set()

            # And it can still be cancelled normally.
            await ws.send(json.dumps({"action": "cancel", "requestId": "turn-1"}))
            cancelled = await _recv_until(ws, "cancelled", [])
            assert cancelled["requestId"] == "turn-1"
    finally:
        await server.shutdown()


async def test_disconnect_mid_turn_aborts_the_turn() -> None:
    probe = SlowToolProbe()
    server = await _start(_slow_tool_mock(), [probe.tool()])
    try:
        async with websockets.connect(server.ws_url()) as ws:
            session_id = await _create_session(ws)
            await _send_message(ws, "turn-x", session_id)
            await asyncio.wait_for(probe.started.wait(), 5)
            # Client hangs up mid-turn.
            await ws.close()

        # The server aborts the in-flight turn: CancelledError fires inside the
        # tool, which never reaches its post-await completion.
        await asyncio.wait_for(probe.cancelled.wait(), 5)
        assert not probe.finished.is_set(), "disconnect must abort the turn before it completes"
    finally:
        await server.shutdown()
