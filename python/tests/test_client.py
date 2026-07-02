"""Client behaviour, driven entirely through a mock transport — no live server.

Covers the core streaming contract: a ``send_message`` turn surfaces
``stream_token`` → ``stream_chunk`` → ``eventual_response`` as typed events in arrival
order, and resolves the turn with the terminal response. Also covers request
correlation for non-streaming actions, error propagation, and HITL resume.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Callable

import pytest

from smooth_operator import (
    ProtocolError,
    SmoothAgentClient,
    Transport,
    TransportState,
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

    # ── test helpers ───────────────────────────────────────────────────────────
    def emit(self, event: dict) -> None:
        data = json.dumps(event)
        for h in list(self._message_handlers):
            h(data)

    def last_sent(self) -> dict:
        return json.loads(self.sent[-1])


def make_client() -> tuple[SmoothAgentClient, MockTransport]:
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
    )
    return client, transport


# ─────────────────────────── streaming ────────────────────────────────────────
async def test_streams_token_chunk_then_eventual_response_in_order_and_resolves() -> None:
    client, transport = make_client()
    await client.connect()

    turn = client.send_message(session_id="sess-1", message="hi", stream=True)
    req_id = transport.last_sent()["requestId"]

    # The outgoing frame is a well-formed send_message action.
    sent = transport.last_sent()
    assert sent["action"] == "send_message"
    assert sent["sessionId"] == "sess-1"
    assert sent["message"] == "hi"

    collected: list = []

    async def iterate() -> None:
        async for ev in turn:
            collected.append(ev)

    task = asyncio.create_task(iterate())
    await asyncio.sleep(0)  # let the iterator start

    transport.emit(
        {"type": "stream_token", "requestId": req_id, "token": "Hel", "data": {"requestId": req_id, "token": "Hel"}}
    )
    transport.emit(
        {"type": "stream_token", "requestId": req_id, "token": "lo", "data": {"requestId": req_id, "token": "lo"}}
    )
    transport.emit(
        {
            "type": "stream_chunk",
            "requestId": req_id,
            "node": "response_composer",
            "data": {
                "requestId": req_id,
                "node": "response_composer",
                "state": {"structuredResponse": {"responseParts": ["Hello"]}},
            },
        }
    )
    transport.emit(
        {
            "type": "eventual_response",
            "requestId": req_id,
            "status": 200,
            "data": {
                "requestId": req_id,
                "status": 200,
                "data": {
                    "messageId": "66666666-6666-6666-6666-666666666666",
                    "response": {"responseParts": ["Hello"]},
                    "needsEscalation": False,
                },
            },
        }
    )

    final = await turn
    await task

    # Terminal response resolves the turn.
    assert final.type == "eventual_response"
    assert str(final.data.data.message_id) == "66666666-6666-6666-6666-666666666666"

    # Events arrived in order through iteration.
    assert [e.type for e in collected] == [
        "stream_token",
        "stream_token",
        "stream_chunk",
        "eventual_response",
    ]
    tokens = "".join(e.token for e in collected if e.type == "stream_token")
    assert tokens == "Hello"


async def test_buffers_tokens_pushed_before_iteration_begins() -> None:
    client, transport = make_client()
    await client.connect()

    turn = client.send_message(session_id="s", message="q")
    req_id = transport.last_sent()["requestId"]

    # Emit before anyone iterates — must be buffered.
    transport.emit(
        {"type": "stream_token", "requestId": req_id, "token": "A", "data": {"requestId": req_id, "token": "A"}}
    )
    transport.emit(
        {
            "type": "eventual_response",
            "requestId": req_id,
            "status": 200,
            "data": {
                "requestId": req_id,
                "status": 200,
                "data": {
                    "messageId": "00000000-0000-0000-0000-000000000001",
                    "response": None,
                },
            },
        }
    )

    types = [ev.type async for ev in turn]
    assert types == ["stream_token", "eventual_response"]


async def test_rejects_turn_on_error_event_with_protocol_error() -> None:
    client, transport = make_client()
    await client.connect()
    turn = client.send_message(session_id="s", message="boom")
    req_id = transport.last_sent()["requestId"]

    transport.emit(
        {
            "type": "error",
            "requestId": req_id,
            "data": {
                "requestId": req_id,
                "error": {"code": "RATE_LIMITED", "message": "slow down"},
            },
        }
    )

    with pytest.raises(ProtocolError) as exc:
        await turn
    assert exc.value.code == "RATE_LIMITED"


async def test_routes_hitl_confirm_resume_back_into_the_same_turn() -> None:
    client, transport = make_client()
    await client.connect()
    turn = client.send_message(session_id="s", message="delete it")
    req_id = transport.last_sent()["requestId"]

    seen: list[str] = []

    async def iterate() -> None:
        async for ev in turn:
            seen.append(ev.type)

    task = asyncio.create_task(iterate())
    await asyncio.sleep(0)

    transport.emit(
        {
            "type": "write_confirmation_required",
            "requestId": req_id,
            "data": {
                "requestId": req_id,
                "data": {"toolId": "t1", "actionDescription": "Delete contact"},
            },
        }
    )

    # Caller approves; the resumed stream completes the original turn.
    client.confirm_tool_action(session_id="22222222-2222-2222-2222-222222222222", request_id=req_id, approved=True)
    sent = transport.last_sent()
    assert sent["action"] == "confirm_tool_action"
    assert sent["approved"] is True
    assert sent["requestId"] == req_id

    transport.emit(
        {
            "type": "eventual_response",
            "requestId": req_id,
            "status": 200,
            "data": {
                "requestId": req_id,
                "status": 200,
                "data": {
                    "messageId": "00000000-0000-0000-0000-000000000002",
                    "response": None,
                },
            },
        }
    )

    await turn
    await task
    assert seen == ["write_confirmation_required", "eventual_response"]


# ─────────────────────────── correlation ──────────────────────────────────────
async def test_create_session_resolves_with_immediate_response_data() -> None:
    client, transport = make_client()
    await client.connect()

    coro = asyncio.create_task(
        client.create_conversation_session(agent_id="11111111-1111-1111-1111-111111111111", user_name="Alice")
    )
    await asyncio.sleep(0)
    req_id = transport.last_sent()["requestId"]
    sent = transport.last_sent()
    assert sent["action"] == "create_conversation_session"
    assert sent["agentId"] == "11111111-1111-1111-1111-111111111111"

    transport.emit(
        {
            "type": "immediate_response",
            "requestId": req_id,
            "status": 200,
            "data": {
                "sessionId": "22222222-2222-2222-2222-222222222222",
                "conversationId": "33333333-3333-3333-3333-333333333333",
                "agentId": "11111111-1111-1111-1111-111111111111",
                "agentName": "Aria",
                "userParticipantId": "44444444-4444-4444-4444-444444444444",
                "agentParticipantId": "55555555-5555-5555-5555-555555555555",
            },
        }
    )

    session = await coro
    assert str(session.session_id) == "22222222-2222-2222-2222-222222222222"
    assert session.agent_name == "Aria"


async def test_ping_resolves_with_the_pong_timestamp() -> None:
    client, transport = make_client()
    await client.connect()
    coro = asyncio.create_task(client.ping())
    await asyncio.sleep(0)
    req_id = transport.last_sent()["requestId"]
    transport.emit({"type": "pong", "requestId": req_id, "timestamp": 1_700_000_000_000})
    assert await coro == 1_700_000_000_000


async def test_does_not_cross_correlate_two_concurrent_requests() -> None:
    client, transport = make_client()
    await client.connect()

    def session_data(sid: str) -> dict:
        return {
            "sessionId": sid,
            "conversationId": "33333333-3333-3333-3333-333333333333",
            "agentId": "11111111-1111-1111-1111-111111111111",
            "agentName": "N",
            "userParticipantId": "44444444-4444-4444-4444-444444444444",
            "agentParticipantId": "55555555-5555-5555-5555-555555555555",
        }

    p1 = asyncio.create_task(client.get_session(session_id="22222222-2222-2222-2222-222222222221"))
    await asyncio.sleep(0)
    req1 = transport.last_sent()["requestId"]
    p2 = asyncio.create_task(client.get_session(session_id="22222222-2222-2222-2222-222222222222"))
    await asyncio.sleep(0)
    req2 = transport.last_sent()["requestId"]
    assert req1 != req2

    # Resolve out of order.
    transport.emit(
        {
            "type": "immediate_response",
            "requestId": req2,
            "status": 200,
            "data": session_data("22222222-2222-2222-2222-222222222222"),
        }
    )
    transport.emit(
        {
            "type": "immediate_response",
            "requestId": req1,
            "status": 200,
            "data": session_data("22222222-2222-2222-2222-222222222221"),
        }
    )

    s1 = await p1
    s2 = await p2
    assert str(s1.session_id) == "22222222-2222-2222-2222-222222222221"
    assert str(s2.session_id) == "22222222-2222-2222-2222-222222222222"


async def test_forwards_uncorrelated_keepalive_to_on_event_listeners() -> None:
    client, transport = make_client()
    await client.connect()
    received: list = []
    client.on_event(received.append)
    transport.emit({"type": "keepalive", "data": {"requestId": "whatever"}})
    assert len(received) == 1
    assert received[0].type == "keepalive"


async def test_rejects_pending_requests_when_transport_closes() -> None:
    client, transport = make_client()
    await client.connect()
    coro = asyncio.create_task(client.get_session(session_id="22222222-2222-2222-2222-222222222222"))
    await asyncio.sleep(0)
    await transport.close()
    with pytest.raises(Exception):
        await coro
