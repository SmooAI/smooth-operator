"""``agentId`` is REQUIRED by the Request schema, so absent-or-blank is a malformed
request — not an agentless session.

Asserts BOTH halves: the request is rejected, and NOTHING is persisted. A rejection that
still writes a row is the same bug wearing an error message.
"""

from __future__ import annotations

import json

import pytest

from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore


async def _dispatch(dispatcher: FrameDispatcher, frame: dict) -> list[dict]:
    events: list[dict] = []
    await dispatcher.dispatch(json.dumps(frame), events.append)
    return events


@pytest.mark.parametrize(
    ("name", "frame"),
    [
        ("absent", {}),
        ("empty", {"agentId": ""}),
        ("whitespace", {"agentId": "   "}),
    ],
)
async def test_rejects_blank_agent_id_without_persisting(name: str, frame: dict) -> None:
    store = InMemorySessionStore()
    events = await _dispatch(
        FrameDispatcher(store, chat_client=None),
        {"action": "create_conversation_session", "requestId": "r1", **frame},
    )

    assert len(events) == 1, (name, events)
    assert events[0]["type"] == "error", (name, events)
    assert events[0]["error"]["code"] == "VALIDATION_ERROR", (name, events)

    # …and no conversation was persisted.
    assert await store.list_conversations(None) == []


async def test_accepts_a_real_agent_id() -> None:
    store = InMemorySessionStore()
    events = await _dispatch(
        FrameDispatcher(store, chat_client=None),
        {"action": "create_conversation_session", "requestId": "r1", "agentId": "agent-1"},
    )
    assert events[0]["type"] == "immediate_response"
    assert events[0]["data"]["agentId"] == "agent-1"
