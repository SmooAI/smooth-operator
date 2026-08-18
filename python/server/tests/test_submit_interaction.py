"""Rich Interactions (``choices`` kind) — the raise → park → ``submit_interaction`` →
resume path, end-to-end over the real Python WS server.

Boots the server with a scripted :class:`~smooth_operator_core.MockLlmProvider` (so the
turn runs offline) and drives the full seam over a real ``websockets`` client:

  - **Rich path** (session declared ``choice_chips``): ``request_choices`` parks the turn
    and the server emits ``interaction_required``; a ``submit_interaction`` action resumes
    it — the raise tool's canonical ``submitted`` payload reaches the model, the turn
    streams the final reply and completes.
  - **Invalid then resubmit**: a bad submit emits ``interaction_invalid`` and the turn
    STAYS parked (retryable, never a terminal ``error``); a corrected submit resumes it.
  - **Fallback path** (no capability): ``request_choices`` returns the conversational
    directive with NO ``interaction_required``; the model submits through the generic
    ``submit_interaction`` *tool* and the turn completes.

The Python analog of the Rust ``submit_interaction`` tests. Like the write-confirmation
test, the ``submit_interaction`` frame arrives on the same connection's reader while the
turn is parked — proving the turn runs as a background task so the reader stays free.
"""

from __future__ import annotations

import json

import websockets
from smooth_operator_core import MockLlmProvider

from smooth_operator_server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore

_QUESTIONS = [
    {"question": "Which plan interests you?", "header": "Plan", "options": [{"label": "Basic"}, {"label": "Pro"}]}
]
_RAISE_ARGS = json.dumps({"questions": _QUESTIONS, "reason": "to route you"})


async def _start(mock: MockLlmProvider) -> tuple:
    state = ServerState(store=InMemorySessionStore(), chat_client=mock)
    server = await serve(state, "127.0.0.1", 0)
    return server, state


async def _create_session(ws, supports: list[str] | None = None) -> str:
    frame = {
        "action": "create_conversation_session",
        "requestId": "r-create",
        "agentId": "11111111-1111-1111-1111-111111111111",
        "userName": "Alice",
        "userEmail": "alice@example.com",
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


async def _send_message(ws) -> None:
    await ws.send(
        json.dumps({"action": "send_message", "requestId": "r-msg", "sessionId": _SID, "message": "help me pick"})
    )


# module-level session id shared across helpers, set per test
_SID = ""


async def test_rich_path_parks_emits_interaction_required_and_resumes() -> None:
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "request_choices", _RAISE_ARGS)
    mock.push_text("Great — I've noted your pick.")
    server, _ = await _start(mock)
    global _SID
    try:
        async with websockets.connect(server.ws_url()) as ws:
            _SID = await _create_session(ws, supports=["choice_chips"])
            await _send_message(ws)

            ack = await _recv(ws)
            assert ack["type"] == "immediate_response" and ack["status"] == 202

            # Park: an interaction_required arrives (a toolCall chunk may precede it).
            event = await _recv(ws)
            if event["type"] == "stream_chunk":
                event = await _recv(ws)
            assert event["type"] == "interaction_required"
            assert event["requestId"] == "r-msg"
            inner = event["data"]["data"]
            assert inner["kind"] == "choices"
            assert inner["spec"]["questions"][0]["header"] == "Plan"
            assert inner["reason"] == "to route you"
            interaction_id = inner["interactionId"]
            assert interaction_id

            # Resume: submit the pick. The reader was free to receive this while parked.
            await ws.send(
                json.dumps(
                    {
                        "action": "submit_interaction",
                        "requestId": "r-msg",
                        "sessionId": _SID,
                        "interactionId": interaction_id,
                        "kind": "choices",
                        "values": {"answers": [{"header": "Plan", "options": ["Pro"]}]},
                    }
                )
            )

            tokens: list[str] = []
            tool_results: list[dict] = []
            saw_submit_ack = False
            while True:
                event = await _recv(ws)
                etype = event["type"]
                if etype == "immediate_response" and event["status"] == 200:
                    saw_submit_ack = True
                    assert event["data"]["values"]["answers"][0]["options"] == ["Pro"]
                elif etype == "stream_chunk":
                    tr = event["data"]["state"].get("rawResponse", {}).get("toolResult")
                    if tr:
                        tool_results.append(tr)
                elif etype == "stream_token":
                    tokens.append(event["token"])
                elif etype == "eventual_response":
                    break

            assert saw_submit_ack, "the submit_interaction ack must arrive"
            assert "".join(tokens) == "Great — I've noted your pick."
            # The raise tool returned the canonical submitted payload to the model.
            payload = json.loads(next(tr["result"] for tr in tool_results if tr["name"] == "request_choices"))
            assert payload["status"] == "submitted"
            assert payload["values"]["answers"][0]["options"] == ["Pro"]
    finally:
        await server.shutdown()


async def test_invalid_submit_stays_parked_then_resubmit_resumes() -> None:
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "request_choices", _RAISE_ARGS)
    mock.push_text("Locked in.")
    server, _ = await _start(mock)
    global _SID
    try:
        async with websockets.connect(server.ws_url()) as ws:
            _SID = await _create_session(ws, supports=["choice_chips"])
            await _send_message(ws)
            assert (await _recv(ws))["status"] == 202
            event = await _recv(ws)
            if event["type"] == "stream_chunk":
                event = await _recv(ws)
            assert event["type"] == "interaction_required"
            interaction_id = event["data"]["data"]["interactionId"]

            # A bad pick (not an offered option) → interaction_invalid, turn stays parked.
            await ws.send(
                json.dumps(
                    {
                        "action": "submit_interaction",
                        "requestId": "r-msg",
                        "sessionId": _SID,
                        "interactionId": interaction_id,
                        "values": {"answers": [{"header": "Plan", "options": ["Platinum"]}]},
                    }
                )
            )
            invalid = await _recv(ws)
            assert invalid["type"] == "interaction_invalid"
            assert invalid["data"]["data"]["errors"][0]["field"] == "Plan"
            assert invalid["data"]["data"]["message"] == "Some fields need attention."

            # Corrected submit → resumes (proves the park survived the invalid attempt).
            await ws.send(
                json.dumps(
                    {
                        "action": "submit_interaction",
                        "requestId": "r-msg",
                        "sessionId": _SID,
                        "interactionId": interaction_id,
                        "values": {"answers": [{"header": "Plan", "options": ["Basic"]}]},
                    }
                )
            )
            while True:
                event = await _recv(ws)
                if event["type"] == "eventual_response":
                    assert event["data"]["data"]["response"]["responseParts"] == ["Locked in."]
                    break
    finally:
        await server.shutdown()


async def test_fallback_path_without_capability_uses_the_submit_tool() -> None:
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "request_choices", _RAISE_ARGS)
    mock.push_tool_call(
        "call-2",
        "submit_interaction",
        json.dumps({"kind": "choices", "values": {"answers": [{"header": "Plan", "options": ["Pro"]}]}}),
    )
    mock.push_text("Thanks for letting me know.")
    server, _ = await _start(mock)
    global _SID
    try:
        async with websockets.connect(server.ws_url()) as ws:
            # No `supports` → text-only channel → choices degrades to the conversational
            # fallback (no card, no park).
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
            # request_choices returned the conversational directive…
            raise_result = json.loads(next(tr["result"] for tr in tool_results if tr["name"] == "request_choices"))
            assert raise_result["mode"] == "conversational"
            assert "submit_interaction" in raise_result["instructions"]
            # …and the generic submit_interaction TOOL returned the submitted payload.
            submit_result = json.loads(next(tr["result"] for tr in tool_results if tr["name"] == "submit_interaction"))
            assert submit_result["status"] == "submitted"
            assert submit_result["values"]["answers"][0]["options"] == ["Pro"]
    finally:
        await server.shutdown()
