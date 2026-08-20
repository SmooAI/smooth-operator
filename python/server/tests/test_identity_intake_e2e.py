"""Rich Interactions (``identity_intake`` kind) — the raise → park → ``submit_interaction``
→ resume path end-to-end over the real Python WS server, PLUS the host effect: a valid submit
stamps the captured contacts onto the session (``user_name`` / ``contact_email`` /
``contact_phone`` — the keys the OTP contact seam reads).

Boots the server with a scripted :class:`~smooth_operator_core.MockLlmProvider` (offline turn)
and drives the full seam over a real ``websockets`` client — the Python analog of the Rust
identity_intake submit tests. Covers the rich path (session declared ``identity_form``, parks
+ resumes + host effect) and the conversational fallback (no capability, generic
``submit_interaction`` tool, SAME host effect).
"""

from __future__ import annotations

import json

import websockets
from smooth_operator_core import MockLlmProvider

from smooth_operator_server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore

_FIELDS = [
    {"key": "name", "required": False},
    {"key": "email", "required": True, "label": "Work email"},
    {"key": "phone", "required": False},
]
_RAISE_ARGS = json.dumps({"fields": _FIELDS, "reason": "to send you the quote"})
_VALUES = {"name": "Alice Example", "email": "alice@Example.com", "phone": "(555) 123-4567"}
# Canonical (normalized) forms the validator produces + stamps.
_NORM = {"name": "Alice Example", "email": "alice@example.com", "phone": "+15551234567"}

_SID = ""


async def _start(mock: MockLlmProvider) -> tuple:
    state = ServerState(store=InMemorySessionStore(), chat_client=mock)
    server = await serve(state, "127.0.0.1", 0)
    return server, state


async def _create_session(ws, supports: list[str] | None = None) -> str:
    frame = {
        "action": "create_conversation_session",
        "requestId": "r-create",
        "agentId": "11111111-1111-1111-1111-111111111111",
    }
    if supports is not None:
        frame["supports"] = supports
    await ws.send(json.dumps(frame))
    while True:
        event = json.loads(await ws.recv())
        if event.get("type") == "immediate_response":
            return event["data"]["sessionId"]


async def _recv(ws):
    while True:
        event = json.loads(await ws.recv())
        if event.get("type") not in ("keepalive", "pong"):
            return event


async def _recv_park(ws):
    """The park event plus the raise tool's deferred toolCall chunk that follows it.

    The reference order is ``interaction_required`` FIRST, then the raise tool's
    ``stream_chunk`` — same as this server's write-confirmation park.
    """
    event = await _recv(ws)
    assert event["type"] == "interaction_required", event
    chunk = await _recv(ws)
    assert chunk["type"] == "stream_chunk", chunk
    return event


async def _send_message(ws) -> None:
    await ws.send(
        json.dumps(
            {"action": "send_message", "requestId": "r-msg", "sessionId": _SID, "message": "here are my details"}
        )
    )


async def test_rich_path_parks_resumes_and_stamps_contacts() -> None:
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "request_identity_intake", _RAISE_ARGS)
    mock.push_text("Thanks — I've got your details.")
    server, state = await _start(mock)
    global _SID
    try:
        async with websockets.connect(server.ws_url()) as ws:
            _SID = await _create_session(ws, supports=["identity_form"])

            # Before the intake, the session has no captured contact (no pre-chat email/name).
            pre = await state.store.get_session(_SID)
            assert pre.contact_email is None and pre.contact_phone is None and pre.user_name is None

            await _send_message(ws)
            ack = await _recv(ws)
            assert ack["type"] == "immediate_response" and ack["status"] == 202

            # Park: interaction_required, then the raise tool's deferred toolCall chunk.
            event = await _recv_park(ws)
            inner = event["data"]["data"]
            assert inner["kind"] == "identity_intake"
            assert inner["spec"]["fields"][1]["key"] == "email"
            assert inner["reason"] == "to send you the quote"
            interaction_id = inner["interactionId"]
            assert interaction_id

            # Resume: submit the (un-normalized) values. The reader was free to receive this.
            await ws.send(
                json.dumps(
                    {
                        "action": "submit_interaction",
                        "requestId": "r-msg",
                        "sessionId": _SID,
                        "interactionId": interaction_id,
                        "kind": "identity_intake",
                        "values": _VALUES,
                    }
                )
            )

            tool_results: list[dict] = []
            tokens: list[str] = []
            saw_submit_ack = False
            while True:
                event = await _recv(ws)
                etype = event["type"]
                if etype == "immediate_response" and event["status"] == 200:
                    saw_submit_ack = True
                    # The ack carries the NORMALIZED values.
                    assert event["data"]["values"] == _NORM
                elif etype == "stream_chunk":
                    tr = event["data"]["state"].get("rawResponse", {}).get("toolResult")
                    if tr:
                        tool_results.append(tr)
                elif etype == "stream_token":
                    tokens.append(event["token"])
                elif etype == "eventual_response":
                    break

            assert saw_submit_ack, "the submit_interaction ack must arrive"
            assert "".join(tokens) == "Thanks — I've got your details."
            # The raise tool returned the canonical submitted payload to the model.
            payload = json.loads(next(tr["result"] for tr in tool_results if tr["name"] == "request_identity_intake"))
            assert payload["status"] == "submitted"
            assert payload["values"] == _NORM

            # HOST EFFECT: the session now carries the captured, normalized contacts — the same
            # keys the OTP contact seam reads (so the session is immediately OTP-verifiable).
            after = await state.store.get_session(_SID)
            assert after.user_name == "Alice Example"
            assert after.contact_email == "alice@example.com"
            assert after.contact_phone == "+15551234567"
    finally:
        await server.shutdown()


async def test_invalid_submit_stays_parked_and_does_not_stamp() -> None:
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "request_identity_intake", _RAISE_ARGS)
    mock.push_text("Got it.")
    server, state = await _start(mock)
    global _SID
    try:
        async with websockets.connect(server.ws_url()) as ws:
            _SID = await _create_session(ws, supports=["identity_form"])
            await _send_message(ws)
            assert (await _recv(ws))["status"] == 202
            event = await _recv_park(ws)
            interaction_id = event["data"]["data"]["interactionId"]

            # A bad email → interaction_invalid, turn stays parked, nothing stamped.
            await ws.send(
                json.dumps(
                    {
                        "action": "submit_interaction",
                        "requestId": "r-msg",
                        "sessionId": _SID,
                        "interactionId": interaction_id,
                        "values": {"email": "not-an-email"},
                    }
                )
            )
            invalid = await _recv(ws)
            assert invalid["type"] == "interaction_invalid"
            assert invalid["data"]["data"]["errors"][0]["field"] == "email"
            # The invalid submit left no contact on the session.
            mid = await state.store.get_session(_SID)
            assert mid.contact_email is None

            # Corrected submit → resumes and stamps (proves the park survived the invalid attempt).
            await ws.send(
                json.dumps(
                    {
                        "action": "submit_interaction",
                        "requestId": "r-msg",
                        "sessionId": _SID,
                        "interactionId": interaction_id,
                        "values": {"email": "alice@example.com"},
                    }
                )
            )
            while True:
                event = await _recv(ws)
                if event["type"] == "eventual_response":
                    break
            after = await state.store.get_session(_SID)
            assert after.contact_email == "alice@example.com"
    finally:
        await server.shutdown()


async def test_fallback_path_submit_tool_also_stamps_contacts() -> None:
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "request_identity_intake", _RAISE_ARGS)
    mock.push_tool_call(
        "call-2",
        "submit_interaction",
        json.dumps({"kind": "identity_intake", "values": {"email": "bob@Example.com"}}),
    )
    mock.push_text("Thanks!")
    server, state = await _start(mock)
    global _SID
    try:
        async with websockets.connect(server.ws_url()) as ws:
            # No `supports` → text-only channel → identity_intake degrades to the conversational
            # fallback (no card, no park); the model submits via the generic tool.
            _SID = await _create_session(ws)
            await _send_message(ws)
            assert (await _recv(ws))["status"] == 202

            events: list[dict] = []
            tool_results: list[dict] = []
            while True:
                event = await _recv(ws)
                events.append(event)
                if event["type"] == "stream_chunk":
                    tr = event["data"]["state"].get("rawResponse", {}).get("toolResult")
                    if tr:
                        tool_results.append(tr)
                if event["type"] == "eventual_response":
                    break

            # The fallback path NEVER emits interaction_required.
            assert all(e["type"] != "interaction_required" for e in events)
            raise_result = json.loads(
                next(tr["result"] for tr in tool_results if tr["name"] == "request_identity_intake")
            )
            assert raise_result["mode"] == "conversational"
            submit_result = json.loads(next(tr["result"] for tr in tool_results if tr["name"] == "submit_interaction"))
            assert submit_result["status"] == "submitted"
            assert submit_result["values"] == {"email": "bob@example.com"}

            # HOST EFFECT fires on the conversational path too.
            after = await state.store.get_session(_SID)
            assert after.contact_email == "bob@example.com"
    finally:
        await server.shutdown()
