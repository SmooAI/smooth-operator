"""Cancelling a turn parked on a Rich Interaction must actually cancel it.

``test_turn_cancel.py`` covers a turn parked in an ordinary tool, whose ``await``
re-raises ``CancelledError`` and so unwinds correctly. This file covers the park
that did NOT: the ``request_<kind>`` raise tool, which caught ``CancelledError``
alongside its own timeout and returned a ``no_response`` result.

That is not a cosmetic difference. asyncio only treats a task as cancelled if the
``CancelledError`` propagates *out* of it — catching it and returning normally
un-cancels the task. So the turn resumed after the client had already been sent the
terminal ``cancelled``: it ran the next model call, persisted an assistant reply,
and emitted an ``eventual_response`` (200) for a requestId the client had been told
ended at 499.

The same swallow lived in the SEP extension host's ``ui/confirm`` park
(``extensions.py``); both now re-raise, and their own ``TimeoutError`` — a genuinely
different thing — still degrades to "no answer" as before.
"""

from __future__ import annotations

import asyncio
import json

import websockets
from smooth_operator_core import MockLlmProvider

from smooth_operator_server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore

_FIELDS = [{"key": "email", "required": True, "label": "Work email"}]
_RAISE_ARGS = json.dumps({"fields": _FIELDS, "reason": "to send you the quote"})
_AGENT_ID = "11111111-1111-1111-1111-111111111111"
# Text the mock replies with IF the turn wrongly resumes. Naming it makes the
# failure message say what actually happened rather than just "extra event".
_RESUMED_REPLY = "I should never have been sent."


async def _create_session(ws, supports: list[str]) -> str:
    await ws.send(
        json.dumps(
            {
                "action": "create_conversation_session",
                "requestId": "r-create",
                "agentId": _AGENT_ID,
                "supports": supports,
            }
        )
    )
    while True:
        event = json.loads(await ws.recv())
        if event.get("type") == "immediate_response":
            return event["data"]["sessionId"]


async def _recv_until(ws, want: str, seen: list, timeout: float = 5.0) -> dict:
    async def _pump() -> dict:
        while True:
            event = json.loads(await ws.recv())
            if event.get("type") == want:
                return event
            seen.append(event)

    return await asyncio.wait_for(_pump(), timeout)


async def _recv_within(ws, timeout: float) -> dict | None:
    try:
        return json.loads(await asyncio.wait_for(ws.recv(), timeout))
    except (TimeoutError, asyncio.TimeoutError, websockets.ConnectionClosed):
        return None


async def test_cancel_while_parked_on_an_interaction_is_terminal() -> None:
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "request_identity_intake", _RAISE_ARGS)
    # Only reached if the swallowed cancellation let the agent loop run on.
    mock.push_text(_RESUMED_REPLY)

    store = InMemorySessionStore()
    state = ServerState(store=store, chat_client=mock)
    server = await serve(state, "127.0.0.1", 0)
    try:
        async with websockets.connect(server.ws_url()) as ws:
            session_id = await _create_session(ws, supports=["identity_form"])
            session = await store.get_session(session_id)
            assert session is not None
            conversation_id = session.conversation_id

            await ws.send(
                json.dumps(
                    {
                        "action": "send_message",
                        "requestId": "turn-1",
                        "sessionId": session_id,
                        "message": "here are my details",
                    }
                )
            )

            # Wait for the park — the turn is now suspended inside the raise tool,
            # which is exactly where the cancellation used to be swallowed.
            seen: list = []
            park = await _recv_until(ws, "interaction_required", seen)
            assert park["data"]["data"]["kind"] == "identity_intake", park

            await ws.send(json.dumps({"action": "cancel", "requestId": "turn-1"}))

            cancelled = await _recv_until(ws, "cancelled", seen)
            assert cancelled["requestId"] == "turn-1", cancelled
            assert cancelled["status"] == 499, cancelled

            # Nothing may follow the terminal event. Without the re-raise this is an
            # `eventual_response` (200) for the same requestId.
            after = await _recv_within(ws, 1.0)
            assert after is None, f"cancelled must be terminal, but got a {after.get('type')!r}: {after}"

            # ...and nothing may be persisted for it either. The user's message was
            # written at the start of the turn and stays; the assistant reply is the
            # one that must never land.
            messages = await store.list_messages(conversation_id, 50)
            outbound = [m for m in messages if m.direction.value == "outbound"]
            assert not outbound, f"a cancelled turn must persist no assistant reply, found: {outbound}"
    finally:
        await server.shutdown()
