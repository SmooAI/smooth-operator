"""Boot + turn round-trip integration tests.

Stand the server up on an ephemeral port (port 0), connect a real ``websockets``
client, and drive the protocol end-to-end with the engine on ``MockLlmProvider`` so
no gateway is needed. Mirrors the C#
``WebSocketProtocolIntegrationTests`` (create_conversation_session → send_message →
stream_chunk(s) + final eventual_response)."""

from __future__ import annotations

import json

import websockets
from smooth_operator_core import MockLlmProvider

from smooth_operator_server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore


async def _start(chat_client=None) -> tuple:
    state = ServerState(store=InMemorySessionStore(), chat_client=chat_client)
    server = await serve(state, "127.0.0.1", 0)
    return server, state


async def _recv_until(ws, type_: str, *, timeout: float = 5.0) -> dict:
    """Read events until one of the given ``type`` arrives (collecting the rest)."""
    while True:
        raw = await ws.recv()
        event = json.loads(raw)
        if event.get("type") == type_:
            return event


async def test_boot_accepts_connection() -> None:
    """``serve()`` starts and accepts a connection; a ping gets a pong."""
    server, _ = await _start()
    try:
        async with websockets.connect(server.ws_url()) as ws:
            await ws.send(json.dumps({"action": "ping", "requestId": "r-ping"}))
            pong = await _recv_until(ws, "pong")
            assert pong["requestId"] == "r-ping"
            assert pong["data"]["timestamp"] == pong["timestamp"]
    finally:
        await server.shutdown()


async def test_turn_round_trip_streams_then_eventual() -> None:
    """create_conversation_session → send_message yields stream_token(s) then the
    terminal eventual_response carrying the engine's reply text."""
    mock = MockLlmProvider()
    mock.push_text("Hello from the engine!")
    server, _ = await _start(chat_client=mock)
    try:
        async with websockets.connect(server.ws_url()) as ws:
            # 1. Create a session.
            await ws.send(
                json.dumps(
                    {
                        "action": "create_conversation_session",
                        "requestId": "r-create",
                        "agentId": "11111111-1111-1111-1111-111111111111",
                        "userName": "Alice",
                        "userEmail": "alice@example.com",
                    }
                )
            )
            created = await _recv_until(ws, "immediate_response")
            session_id = created["data"]["sessionId"]
            assert created["status"] == 200
            assert session_id

            # 2. Send a message.
            await ws.send(
                json.dumps(
                    {
                        "action": "send_message",
                        "requestId": "r-msg",
                        "sessionId": session_id,
                        "message": "hi there",
                    }
                )
            )

            # 3. Collect the stream: a 202 ack, stream_token deltas, then the
            #    terminal eventual_response.
            tokens: list[str] = []
            saw_ack = False
            while True:
                event = json.loads(await ws.recv())
                etype = event["type"]
                if etype == "immediate_response" and event["status"] == 202:
                    saw_ack = True
                elif etype == "stream_token":
                    tokens.append(event["token"])
                elif etype == "eventual_response":
                    assert saw_ack, "expected the 202 ack before the stream"
                    assert event["status"] == 200
                    inner = event["data"]["data"]
                    assert inner["response"]["responseParts"] == ["Hello from the engine!"]
                    assert inner["needsEscalation"] is False
                    assert inner["messageId"]
                    break

            assert "".join(tokens) == "Hello from the engine!"
            assert len(tokens) > 1, "the engine should chunk the reply into multiple deltas"
    finally:
        await server.shutdown()


async def test_send_message_to_unknown_session_errors() -> None:
    """A send_message for a session that was never created returns a clean error."""
    mock = MockLlmProvider()
    mock.push_text("unused")
    server, _ = await _start(chat_client=mock)
    try:
        async with websockets.connect(server.ws_url()) as ws:
            await ws.send(
                json.dumps(
                    {
                        "action": "send_message",
                        "requestId": "r-msg",
                        "sessionId": "does-not-exist",
                        "message": "hi",
                    }
                )
            )
            err = await _recv_until(ws, "error")
            assert err["error"]["code"] == "SESSION_NOT_FOUND"
            assert err["requestId"] == "r-msg"
    finally:
        await server.shutdown()


async def test_send_message_without_gateway_errors_cleanly() -> None:
    """With no chat client, send_message returns LLM_UNAVAILABLE — the server stays
    usable for protocol-only checks."""
    server, _ = await _start(chat_client=None)
    try:
        async with websockets.connect(server.ws_url()) as ws:
            await ws.send(json.dumps({"action": "create_conversation_session", "requestId": "r-c"}))
            created = await _recv_until(ws, "immediate_response")
            session_id = created["data"]["sessionId"]
            await ws.send(
                json.dumps(
                    {
                        "action": "send_message",
                        "requestId": "r-m",
                        "sessionId": session_id,
                        "message": "hi",
                    }
                )
            )
            err = await _recv_until(ws, "error")
            assert err["error"]["code"] == "LLM_UNAVAILABLE"
    finally:
        await server.shutdown()
