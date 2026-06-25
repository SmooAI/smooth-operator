"""Write-confirmation HITL — the pause → ``confirm_tool_action`` → resume path.

Boots the real Python WS server with a confirmation-gated tool and a scripted
:class:`~smooth_operator_core.MockLlmProvider` (so the turn runs offline), then
drives the full seam end-to-end over a real ``websockets`` client:

  - **Approve** → the parked tool runs; its result reaches the model (a
    ``stream_chunk`` with the tool result), the turn streams the final reply and
    completes with an ``eventual_response``.
  - **Reject** → the tool is blocked; the model sees a ``Denied by human`` result
    instead, and the turn still completes (no hang).

The Python analog of the Rust ``tests/confirm_tool_action.rs``. The ``confirm_tool_action``
frame arrives on the same connection's reader while the turn is parked — proving the
turn runs as a background task (not awaited inline), so the reader stays free to
receive the confirmation. Also covers the fail-closed validation
(``NO_PENDING_CONFIRMATION`` / ``VALIDATION_ERROR``).
"""

from __future__ import annotations

import json

import websockets
from smooth_operator_core import FunctionTool, MockLlmProvider

from smooth_operator_server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore

SESSION_TOOL = "delete_record"


def _gated_tool() -> FunctionTool:
    async def _fn(_args: dict) -> str:
        return "Record 42 deleted."

    return FunctionTool(
        name=SESSION_TOOL,
        description="Delete a record by id (a state-mutating write).",
        parameters={"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
        func=_fn,
    )


def _scripted_mock() -> MockLlmProvider:
    """Turn 1 calls the gated tool; turn 2 wraps up with a final reply."""
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", SESSION_TOOL, '{"id": "42"}')
    mock.push_text("Done — record 42 was deleted.")
    return mock


async def _start(confirm_tools: list[str]) -> tuple:
    state = ServerState(
        store=InMemorySessionStore(),
        chat_client=_scripted_mock(),
        tools=[_gated_tool()],
        confirm_tools=confirm_tools,
    )
    server = await serve(state, "127.0.0.1", 0)
    return server, state


async def _create_session(ws) -> str:
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
    while True:
        event = json.loads(await ws.recv())
        if event.get("type") == "immediate_response":
            return event["data"]["sessionId"]


async def _recv(ws):
    while True:
        event = json.loads(await ws.recv())
        if event.get("type") not in ("keepalive", "pong"):
            return event


async def test_approved_confirmation_runs_the_gated_tool_and_completes() -> None:
    server, _ = await _start([SESSION_TOOL])
    try:
        async with websockets.connect(server.ws_url()) as ws:
            session_id = await _create_session(ws)

            # send_message: the gated tool call parks the turn → 202 ack then a
            # write_confirmation_required event (and nothing else until we confirm).
            await ws.send(
                json.dumps(
                    {
                        "action": "send_message",
                        "requestId": "r-msg",
                        "sessionId": session_id,
                        "message": "delete record 42",
                    }
                )
            )
            ack = await _recv(ws)
            assert ack["type"] == "immediate_response" and ack["status"] == 202

            confirm = await _recv(ws)
            # If a stream_chunk(toolCall) arrives first, accept it (the engine emits
            # the requested-call chunk before the gate parks); the next is the prompt.
            if confirm["type"] == "stream_chunk":
                confirm = await _recv(ws)
            assert confirm["type"] == "write_confirmation_required"
            assert confirm["requestId"] == "r-msg"
            assert confirm["data"]["data"]["toolId"] == SESSION_TOOL
            assert confirm["data"]["data"]["actionDescription"]

            # Confirm: approve. The reader was free to receive THIS frame while the
            # turn was parked — proving the turn runs as a background task.
            await ws.send(
                json.dumps(
                    {
                        "action": "confirm_tool_action",
                        "requestId": "r-confirm",
                        "sessionId": session_id,
                        "approved": True,
                    }
                )
            )

            # Collect the resumed stream: the confirm ack, the tool result chunk, the
            # final tokens, and the terminal eventual_response.
            tokens: list[str] = []
            tool_results: list[dict] = []
            saw_ack = False
            while True:
                event = await _recv(ws)
                etype = event["type"]
                if etype == "immediate_response" and event["status"] == 200:
                    saw_ack = True
                    assert event["data"]["approved"] is True
                elif etype == "stream_chunk":
                    state = event["data"]["state"]
                    tr = state.get("rawResponse", {}).get("toolResult")
                    if tr:
                        tool_results.append(tr)
                elif etype == "stream_token":
                    tokens.append(event["token"])
                elif etype == "eventual_response":
                    assert event["status"] == 200
                    inner = event["data"]["data"]
                    assert inner["response"]["responseParts"] == ["Done — record 42 was deleted."]
                    break

            assert saw_ack, "the confirm_tool_action ack must arrive"
            assert "".join(tokens) == "Done — record 42 was deleted."
            # The approved tool actually ran — its real result reached the model.
            assert any(tr["name"] == SESSION_TOOL and "deleted" in tr["result"] for tr in tool_results), tool_results
            assert all("Denied by human" not in tr["result"] for tr in tool_results)
    finally:
        await server.shutdown()


async def test_rejected_confirmation_blocks_the_tool_but_turn_completes() -> None:
    server, _ = await _start([SESSION_TOOL])
    try:
        async with websockets.connect(server.ws_url()) as ws:
            session_id = await _create_session(ws)
            await ws.send(
                json.dumps(
                    {"action": "send_message", "requestId": "r-msg", "sessionId": session_id, "message": "delete it"}
                )
            )
            ack = await _recv(ws)
            assert ack["type"] == "immediate_response" and ack["status"] == 202
            confirm = await _recv(ws)
            if confirm["type"] == "stream_chunk":
                confirm = await _recv(ws)
            assert confirm["type"] == "write_confirmation_required"

            # Reject → the engine feeds the model a "Denied by human" result; the
            # tool never runs, but the turn still completes.
            await ws.send(
                json.dumps(
                    {
                        "action": "confirm_tool_action",
                        "requestId": "r-confirm",
                        "sessionId": session_id,
                        "approved": False,
                    }
                )
            )

            tool_results: list[dict] = []
            saw_reject_ack = False
            while True:
                event = await _recv(ws)
                etype = event["type"]
                if etype == "immediate_response" and event["status"] == 200:
                    saw_reject_ack = True
                    assert event["data"]["approved"] is False
                elif etype == "stream_chunk":
                    tr = event["data"]["state"].get("rawResponse", {}).get("toolResult")
                    if tr:
                        tool_results.append(tr)
                elif etype == "eventual_response":
                    break

            assert saw_reject_ack
            # The rejected tool was blocked — the model saw a denial, not the result.
            assert any("Denied by human" in tr["result"] for tr in tool_results), tool_results
            assert all("Record 42 deleted" not in tr["result"] for tr in tool_results)
    finally:
        await server.shutdown()


async def test_confirm_without_pending_is_a_clean_error() -> None:
    """A confirm for a session with no parked turn → NO_PENDING_CONFIRMATION (never
    silently approves)."""
    server, _ = await _start([SESSION_TOOL])
    try:
        async with websockets.connect(server.ws_url()) as ws:
            session_id = await _create_session(ws)
            await ws.send(
                json.dumps(
                    {
                        "action": "confirm_tool_action",
                        "requestId": "r-confirm",
                        "sessionId": session_id,
                        "approved": True,
                    }
                )
            )
            err = await _recv(ws)
            assert err["type"] == "error"
            assert err["error"]["code"] == "NO_PENDING_CONFIRMATION"
    finally:
        await server.shutdown()


async def test_confirm_with_non_bool_approved_fails_closed() -> None:
    """A confirm whose ``approved`` is not a boolean is rejected as a VALIDATION_ERROR
    — a garbled verdict must never be read as an approval."""
    server, _ = await _start([SESSION_TOOL])
    try:
        async with websockets.connect(server.ws_url()) as ws:
            session_id = await _create_session(ws)
            await ws.send(
                json.dumps(
                    {
                        "action": "confirm_tool_action",
                        "requestId": "r-confirm",
                        "sessionId": session_id,
                        "approved": "yes",
                    }
                )
            )
            err = await _recv(ws)
            assert err["type"] == "error"
            assert err["error"]["code"] == "VALIDATION_ERROR"
    finally:
        await server.shutdown()
