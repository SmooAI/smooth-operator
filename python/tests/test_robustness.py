"""Robustness regressions found in an adversarial review of the Python client:

* Bug 2: a streaming turn whose server accepts ``send_message`` but never emits a
  terminal ``eventual_response`` / ``error`` hung forever. It must now settle with a
  :class:`TurnTimeoutError` within the bound (await raises it; ``async for`` re-raises).
* Bug 3: the turn / ``_request`` futures were bound via ``asyncio.get_event_loop()``,
  which off the running loop can attach to a loop that never runs → silent hang. They
  must bind to the *running* loop so they resolve on it.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Callable

import pytest

from smooth_operator import (
    SmoothAgentClient,
    Transport,
    TransportState,
    TurnTimeoutError,
)


class MockTransport(Transport):
    """In-memory transport: captures sent frames, lets the test inject server events."""

    def __init__(self) -> None:
        self._state: TransportState = "closed"
        self.sent: list[str] = []
        self._message_handlers: set[Callable[[str], None]] = set()
        self._close_handlers: set[Callable[[dict], None]] = set()
        self._error_handlers: set[Callable[[object], None]] = set()

    @property
    def state(self) -> TransportState:
        return self._state

    async def connect(self) -> None:
        self._state = "open"

    def send(self, data: str) -> None:
        if self._state != "open":
            raise RuntimeError(f"not open: {self._state}")
        self.sent.append(data)

    async def close(self, code: int = 1000, reason: str = "") -> None:
        self._state = "closed"
        for h in list(self._close_handlers):
            h({"code": code, "reason": reason})

    def on_message(self, handler: Callable[[str], None]) -> Callable[[], None]:
        self._message_handlers.add(handler)
        return lambda: self._message_handlers.discard(handler)

    def on_close(self, handler: Callable[[dict], None]) -> Callable[[], None]:
        self._close_handlers.add(handler)
        return lambda: self._close_handlers.discard(handler)

    def on_error(self, handler: Callable[[object], None]) -> Callable[[], None]:
        self._error_handlers.add(handler)
        return lambda: self._error_handlers.discard(handler)

    def emit(self, event: dict) -> None:
        data = json.dumps(event)
        for h in list(self._message_handlers):
            h(data)

    def last_sent(self) -> dict:
        return json.loads(self.sent[-1])


def make_client(turn_timeout: float = 0.0) -> tuple[SmoothAgentClient, MockTransport]:
    transport = MockTransport()
    counter = {"n": 0}

    def gen() -> str:
        counter["n"] += 1
        return f"req-test-{counter['n']}"

    client = SmoothAgentClient(
        url="wss://test",
        transport=transport,
        generate_request_id=gen,
        request_timeout=1.0,
        turn_timeout=turn_timeout,
    )
    return client, transport


# ─────────────────────────── Bug 2 — turn timeout ─────────────────────────────────
async def test_streaming_turn_times_out_when_no_terminal_event_arrives() -> None:
    client, transport = make_client(turn_timeout=0.05)
    await client.connect()

    turn = client.send_message(session_id="s", message="hang")
    req_id = transport.last_sent()["requestId"]

    # An intermediate event arrives, but the server never sends a terminal one.
    transport.emit(
        {
            "type": "stream_token",
            "requestId": req_id,
            "token": "partial",
            "data": {"requestId": req_id, "token": "partial"},
        }
    )

    loop = asyncio.get_running_loop()
    start = loop.time()
    with pytest.raises(TurnTimeoutError) as exc:
        await turn
    assert exc.value.request_id == req_id
    # Settled well within a generous bound (the timeout is 50ms).
    assert loop.time() - start < 2.0


async def test_streaming_turn_timeout_surfaces_through_async_iteration() -> None:
    client, transport = make_client(turn_timeout=0.05)
    await client.connect()

    turn = client.send_message(session_id="s", message="hang")

    with pytest.raises(TurnTimeoutError):
        async for _ev in turn:
            pass


async def test_streaming_turn_timeout_does_not_fire_after_terminal_event() -> None:
    client, transport = make_client(turn_timeout=0.05)
    await client.connect()

    turn = client.send_message(session_id="s", message="ok")
    req_id = transport.last_sent()["requestId"]

    transport.emit(
        {
            "type": "eventual_response",
            "requestId": req_id,
            "status": 200,
            "data": {
                "requestId": req_id,
                "status": 200,
                "data": {
                    "messageId": "00000000-0000-0000-0000-000000000003",
                    "response": None,
                },
            },
        }
    )

    final = await turn
    assert final.type == "eventual_response"

    # Wait past the timeout window; the turn must stay resolved, not flip to error.
    await asyncio.sleep(0.08)
    again = await turn
    assert again.type == "eventual_response"


# ─────────────────────────── Bug 3 — running-loop binding ──────────────────────────
async def test_turn_future_resolves_on_the_running_loop() -> None:
    """The turn's settled future must be bound to the running loop, so resolving it
    from a dispatcher callback on that same loop wakes the awaiting coroutine."""
    client, transport = make_client()
    await client.connect()

    running = asyncio.get_running_loop()
    turn = client.send_message(session_id="s", message="hi")

    # The settled future is bound to the loop that is actually running.
    assert turn._settled.get_loop() is running  # noqa: SLF001 - asserting the fix

    req_id = transport.last_sent()["requestId"]
    transport.emit(
        {
            "type": "eventual_response",
            "requestId": req_id,
            "status": 200,
            "data": {
                "requestId": req_id,
                "status": 200,
                "data": {
                    "messageId": "00000000-0000-0000-0000-000000000004",
                    "response": None,
                },
            },
        }
    )

    # Resolves promptly on the running loop (would hang if bound to a dead loop).
    final = await asyncio.wait_for(turn, timeout=1.0)
    assert final.type == "eventual_response"


async def test_request_future_resolves_on_the_running_loop() -> None:
    """The non-streaming _request future must also bind to the running loop."""
    client, transport = make_client()
    await client.connect()

    coro = asyncio.create_task(client.ping())
    await asyncio.sleep(0)
    req_id = transport.last_sent()["requestId"]
    transport.emit({"type": "pong", "requestId": req_id, "timestamp": 1_700_000_000_000})

    assert await asyncio.wait_for(coro, timeout=1.0) == 1_700_000_000_000
