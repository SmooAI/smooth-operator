"""``get_conversation_messages`` (pearl th-89b698).

Mirrors the merged Go reference (`go/server/get_messages_test.go`) and the
``spec/actions/get-messages.schema.json`` contract: messages NEWEST-first as
``{id, direction, content: {text}, createdAt}`` plus ``hasMore``, with ``limit``
(1..100, default 50) and an optional ISO 8601 ``before`` cursor.

Driven through the real :class:`FrameDispatcher`, like the list_conversations tests.
"""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone

import pytest

from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore, MessageDirection, StoredMessage


async def _dispatch(dispatcher: FrameDispatcher, frame: dict) -> list[dict]:
    """Dispatch one frame, collecting every event emitted to the sink."""
    events: list[dict] = []
    await dispatcher.dispatch(json.dumps(frame), events.append)
    return events


async def _get_messages(store: InMemorySessionStore, **frame: object) -> dict:
    """Drive one get_conversation_messages frame and return its single event."""
    events = await _dispatch(FrameDispatcher(store, chat_client=None), {"action": "get_conversation_messages", **frame})
    assert len(events) == 1, events
    return events[0]


@pytest.mark.asyncio
async def test_returns_messages_newest_first() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    await store.append_message(session.conversation_id, MessageDirection.INBOUND, "first")
    await store.append_message(session.conversation_id, MessageDirection.OUTBOUND, "second")

    event = await _get_messages(store, requestId="r1", sessionId=session.session_id)

    data = event["data"]
    assert data["hasMore"] is False
    messages = data["messages"]
    assert len(messages) == 2
    # Newest-first: the outbound "second" leads.
    assert [m["direction"] for m in messages] == ["outbound", "inbound"]
    assert messages[0]["content"] == {"text": "second"}
    assert messages[0]["id"]
    # createdAt is ISO 8601 and round-trips.
    assert datetime.fromisoformat(messages[0]["createdAt"]).tzinfo is not None


@pytest.mark.asyncio
async def test_unknown_session_is_not_found() -> None:
    event = await _get_messages(InMemorySessionStore(), requestId="r1", sessionId="nope")
    assert event["error"]["code"] == "SESSION_NOT_FOUND"


@pytest.mark.asyncio
async def test_missing_session_id_is_validation_error() -> None:
    event = await _get_messages(InMemorySessionStore(), requestId="r1")
    assert event["error"]["code"] == "VALIDATION_ERROR"


@pytest.mark.asyncio
async def test_limit_below_message_count_sets_has_more() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    for text in ("m1", "m2", "m3", "m4"):
        await store.append_message(session.conversation_id, MessageDirection.INBOUND, text)

    data = (await _get_messages(store, requestId="r1", sessionId=session.session_id, limit=2))["data"]

    assert data["hasMore"] is True
    assert [m["content"]["text"] for m in data["messages"]] == ["m4", "m3"]


@pytest.mark.asyncio
async def test_limit_is_clamped_to_100() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    for i in range(105):
        await store.append_message(session.conversation_id, MessageDirection.INBOUND, f"m{i}")

    data = (await _get_messages(store, requestId="r1", sessionId=session.session_id, limit=500))["data"]

    assert len(data["messages"]) == 100
    assert data["hasMore"] is True


@pytest.mark.asyncio
async def test_before_cursor_filters_strictly_older() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    # Stamp explicitly — back-to-back appends can land on the same clock tick.
    base = datetime(2026, 7, 18, 12, 0, 0, tzinfo=timezone.utc)
    for i in range(3):
        await store.append_message(session.conversation_id, MessageDirection.INBOUND, f"m{i}")
    store._messages[session.conversation_id] = [
        StoredMessage(m.id, m.conversation_id, m.direction, m.text, base + timedelta(minutes=i))
        for i, m in enumerate(store._messages[session.conversation_id])
    ]

    data = (
        await _get_messages(
            store,
            requestId="r1",
            sessionId=session.session_id,
            before=(base + timedelta(minutes=2)).isoformat(),
        )
    )["data"]

    # m2 sits exactly at the cursor — `before` is strict, so only m1/m0 come back.
    assert [m["content"]["text"] for m in data["messages"]] == ["m1", "m0"]
    assert data["hasMore"] is False


@pytest.mark.asyncio
async def test_invalid_before_is_validation_error() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    event = await _get_messages(store, requestId="r1", sessionId=session.session_id, before="not-a-timestamp")
    assert event["error"]["code"] == "VALIDATION_ERROR"
