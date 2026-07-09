"""``list_conversations`` + resume-by-``conversationId`` (pearl th-d5b446).

Mirrors the merged Rust/Go/TS reference: ``list_conversations`` rolls up
conversations most-recent-first, drops empties, and derives a clean title from the
first inbound (user) message; a ``create_conversation_session`` carrying a known
``conversationId`` binds to (resumes) that conversation, while an unknown/absent id
mints a fresh one.

Driven through the real :class:`FrameDispatcher` (like the OTP flow tests) plus fast
unit cases against the store + the title helper.
"""

from __future__ import annotations

import json

import pytest

from smooth_operator_server.dispatcher import FrameDispatcher, _conversation_title
from smooth_operator_server.session_store import InMemorySessionStore, MessageDirection


async def _dispatch(dispatcher: FrameDispatcher, frame: dict) -> list[dict]:
    """Dispatch one frame, collecting every event emitted to the sink."""
    events: list[dict] = []
    await dispatcher.dispatch(json.dumps(frame), events.append)
    return events


# --------------------------------------------------------------------------- #
# _conversation_title helper
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    ("first", "fallback", "want"),
    [
        ("Hello there", "fb", "Hello there"),
        ("   spaced   ", "fb", "spaced"),
        ("### Big title", "fb", "Big title"),
        ("- do the thing", "fb", "do the thing"),
        ("> _quoted_ line", "fb", "quoted_ line"),
        ("", "My Conversation", "My Conversation"),
        (None, "My Conversation", "My Conversation"),
        ("###   ", "Fallback", "Fallback"),
        (
            "012345678901234567890123456789012345678901234567890123456789EXTRA",
            "fb",
            "012345678901234567890123456789012345678901234567890123456789…",
        ),
    ],
)
def test_conversation_title(first: str | None, fallback: str, want: str) -> None:
    assert _conversation_title(first, fallback) == want


def test_conversation_title_truncates_to_60_plus_ellipsis() -> None:
    got = _conversation_title("x" * 100, "fb")
    assert len(got) == 61  # 60 chars + the ellipsis rune
    assert got.endswith("…")


# --------------------------------------------------------------------------- #
# store: resume + list_conversations
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_list_conversations_filters_empties_and_previews_title() -> None:
    store = InMemorySessionStore()

    # A: empty conversation (created, never messaged) → excluded.
    await store.create_session("agent", "U", "u@example.com")
    # B: has messages → included, title from first inbound.
    b = await store.create_session("agent", "U", "u@example.com")
    await store.append_message(b.conversation_id, MessageDirection.INBOUND, "## First user line")
    await store.append_message(b.conversation_id, MessageDirection.OUTBOUND, "agent reply")

    summaries = await store.list_conversations()
    assert len(summaries) == 1
    got = summaries[0]
    assert got.conversation_id == b.conversation_id
    assert got.message_count == 2
    assert got.first_inbound_text == "## First user line"
    assert got.updated_at is not None


@pytest.mark.asyncio
async def test_resume_binds_to_existing_conversation() -> None:
    store = InMemorySessionStore()
    first = await store.create_session("agent", None, None)
    await store.append_message(first.conversation_id, MessageDirection.INBOUND, "original")

    # Resume: a new session bound to the same conversation keeps the log.
    resumed = await store.create_session("agent", None, None, first.conversation_id)
    assert resumed.conversation_id == first.conversation_id
    assert resumed.session_id != first.session_id
    log = await store.list_messages(first.conversation_id, 100)
    assert [m.text for m in log] == ["original"]  # history preserved, not reset


@pytest.mark.asyncio
async def test_unknown_conversation_id_mints_fresh() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", None, None, "does-not-exist")
    assert session.conversation_id != "does-not-exist"
    # A brand-new empty conversation → nothing to list.
    assert await store.list_conversations() == []


# --------------------------------------------------------------------------- #
# dispatcher: list_conversations action + resume via create_conversation_session
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_dispatch_list_conversations_sorted_and_capped() -> None:
    store = InMemorySessionStore()
    dispatcher = FrameDispatcher(store, None)

    older = await store.create_session("agent", None, None)
    await store.append_message(older.conversation_id, MessageDirection.INBOUND, "older")
    newer = await store.create_session("agent", None, None)
    await store.append_message(newer.conversation_id, MessageDirection.INBOUND, "newer")
    # Touch `older` again so it becomes the most recently active.
    await store.append_message(older.conversation_id, MessageDirection.OUTBOUND, "older reply")

    events = await _dispatch(dispatcher, {"action": "list_conversations", "requestId": "r1"})
    assert len(events) == 1
    ev = events[0]
    assert ev["type"] == "immediate_response"
    assert ev["status"] == 200
    convos = ev["data"]["conversations"]
    assert [c["conversationId"] for c in convos] == [older.conversation_id, newer.conversation_id]
    assert convos[0]["title"] == "older"
    assert convos[0]["messageCount"] == 2

    # limit caps the result.
    capped = await _dispatch(dispatcher, {"action": "list_conversations", "requestId": "r2", "limit": 1})
    assert len(capped[0]["data"]["conversations"]) == 1


@pytest.mark.asyncio
async def test_dispatch_resume_echoes_conversation_id_and_keeps_history() -> None:
    store = InMemorySessionStore()
    dispatcher = FrameDispatcher(store, None)

    first = await store.create_session("agent", None, None)
    await store.append_message(first.conversation_id, MessageDirection.INBOUND, "hi")

    events = await _dispatch(
        dispatcher,
        {"action": "create_conversation_session", "requestId": "r1", "conversationId": first.conversation_id},
    )
    data = events[0]["data"]
    assert data["conversationId"] == first.conversation_id  # resumed: same id echoed back
    assert data["sessionId"] != first.session_id
    # History preserved (resume did not reset the log).
    log = await store.list_messages(first.conversation_id, 100)
    assert [m.text for m in log] == ["hi"]


@pytest.mark.asyncio
async def test_dispatch_create_without_conversation_id_mints_fresh() -> None:
    store = InMemorySessionStore()
    dispatcher = FrameDispatcher(store, None)

    events = await _dispatch(dispatcher, {"action": "create_conversation_session", "requestId": "r1"})
    convo_id = events[0]["data"]["conversationId"]
    assert isinstance(convo_id, str) and convo_id
    # Fresh empty conversation → not listed.
    assert await store.list_conversations() == []
