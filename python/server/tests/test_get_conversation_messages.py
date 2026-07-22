"""``get_conversation_messages`` (pearl th-89b698).

Mirrors the merged Go reference (`go/server/get_messages_test.go`) and the
``spec/actions/get-messages.schema.json`` contract: messages NEWEST-first as
``{id, direction, content: {text}, createdAt}`` plus ``nextCursor``/``hasMore``, with
``limit`` (1..100, default 50) and an optional opaque ``cursor`` (th-ebc251).

Driven through the real :class:`FrameDispatcher`, like the list_conversations tests.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone

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


async def _restamp(store: InMemorySessionStore, conversation_id: str, stamps: list[datetime]) -> None:
    """Force each stored message's ``created_at`` — back-to-back appends otherwise land on
    whatever the clock hands out, which is exactly what these tests need to control."""
    store._messages[conversation_id] = [
        StoredMessage(m.id, m.conversation_id, m.direction, m.text, stamp)
        for m, stamp in zip(store._messages[conversation_id], stamps, strict=True)
    ]


@pytest.mark.asyncio
async def test_cursor_returns_the_page_after_the_named_message() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    for i in range(3):
        await store.append_message(session.conversation_id, MessageDirection.INBOUND, f"m{i}")

    page1 = (await _get_messages(store, requestId="r1", sessionId=session.session_id, limit=1))["data"]
    assert [m["content"]["text"] for m in page1["messages"]] == ["m2"]
    assert page1["hasMore"] is True
    # nextCursor names the OLDEST message in the page — here the only one.
    assert page1["nextCursor"] == page1["messages"][0]["id"]

    page2 = (
        await _get_messages(store, requestId="r1", sessionId=session.session_id, limit=1, cursor=page1["nextCursor"])
    )["data"]
    # Strictly older than the cursor — the cursor's own message is not repeated.
    assert [m["content"]["text"] for m in page2["messages"]] == ["m1"]


@pytest.mark.asyncio
async def test_paging_visits_every_message_exactly_once() -> None:
    """Follow nextCursor to exhaustion at limit=1: every message once, newest-first, and
    hasMore/nextCursor both fall away on the final page."""
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    for i in range(5):
        await store.append_message(session.conversation_id, MessageDirection.INBOUND, f"m{i}")

    seen: list[str] = []
    cursor: str | None = None
    for _ in range(10):  # Loop bound: a paging bug must fail the test, not hang it.
        frame = {"requestId": "r1", "sessionId": session.session_id, "limit": 1}
        if cursor is not None:
            frame["cursor"] = cursor
        data = (await _get_messages(store, **frame))["data"]
        seen.extend(m["content"]["text"] for m in data["messages"])
        if not data["hasMore"]:
            assert data["nextCursor"] is None
            break
        assert data["nextCursor"] is not None
        cursor = data["nextCursor"]
    else:
        pytest.fail("paging never terminated")

    assert seen == ["m4", "m3", "m2", "m1", "m0"]


@pytest.mark.asyncio
async def test_identical_timestamps_are_neither_dropped_nor_duplicated() -> None:
    """The collision an id cursor exists to survive: two messages sharing a created_at to
    the microsecond, paged at limit=1. A `created_at < cursor` filter returns the older
    one twice (non-strict) or loses it entirely (strict) — Go shipped exactly that bug.
    th-ebc251."""
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    for i in range(2):
        await store.append_message(session.conversation_id, MessageDirection.INBOUND, f"m{i}")
    same = datetime(2026, 7, 18, 12, 0, 0, 123_456, tzinfo=timezone.utc)
    await _restamp(store, session.conversation_id, [same, same])

    page1 = (await _get_messages(store, requestId="r1", sessionId=session.session_id, limit=1))["data"]
    assert [m["content"]["text"] for m in page1["messages"]] == ["m1"]
    assert page1["hasMore"] is True

    page2 = (
        await _get_messages(store, requestId="r1", sessionId=session.session_id, limit=1, cursor=page1["nextCursor"])
    )["data"]
    assert [m["content"]["text"] for m in page2["messages"]] == ["m0"]
    assert page2["hasMore"] is False
    assert page2["nextCursor"] is None


@pytest.mark.asyncio
async def test_created_at_keeps_sub_second_precision() -> None:
    """createdAt is display-only now, but it still must not be truncated to whole seconds
    (Go's original RFC3339 bug). th-89b698."""
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    await store.append_message(session.conversation_id, MessageDirection.INBOUND, "m0")
    stamp = datetime(2026, 7, 18, 12, 0, 0, 123_456, tzinfo=timezone.utc)
    await _restamp(store, session.conversation_id, [stamp])

    data = (await _get_messages(store, requestId="r1", sessionId=session.session_id))["data"]

    created_at = data["messages"][0]["createdAt"]
    assert datetime.fromisoformat(created_at) == stamp
    assert datetime.fromisoformat(created_at).microsecond == 123_456


@pytest.mark.asyncio
async def test_unknown_cursor_is_validation_error() -> None:
    """A stale or fabricated cursor is a clean error, not a silently empty page."""
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    await store.append_message(session.conversation_id, MessageDirection.INBOUND, "m0")

    event = await _get_messages(store, requestId="r1", sessionId=session.session_id, cursor="no-such-message-id")

    assert event["error"]["code"] == "VALIDATION_ERROR"


@pytest.mark.asyncio
async def test_cursor_on_the_oldest_message_returns_an_empty_final_page() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", "U", "u@example.com")
    for i in range(2):
        await store.append_message(session.conversation_id, MessageDirection.INBOUND, f"m{i}")
    oldest = store._messages[session.conversation_id][0].id

    data = (await _get_messages(store, requestId="r1", sessionId=session.session_id, cursor=oldest))["data"]

    assert data["messages"] == []
    assert data["hasMore"] is False
    assert data["nextCursor"] is None
