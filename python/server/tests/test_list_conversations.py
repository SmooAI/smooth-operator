"""``list_conversations`` + resume-by-conversationId — the conversation-sidebar /
resume substrate.

Drives the :class:`FrameDispatcher` directly with a capturing sink and an
:class:`InMemorySessionStore` (no socket, no LLM needed for the protocol-only
paths). The Python analog of the Rust ``handle_list_conversations`` +
``handle_create_session`` resume branch. Covers: empties filtered, title preview
(first inbound, markdown-stripped, truncated), name fallback, most-recent-first
ordering, limit, resume binds to an existing conversation, and an unknown id
falling back to a fresh conversation.
"""

from __future__ import annotations

import asyncio
import json

from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore, MessageDirection


def _dispatcher(store: InMemorySessionStore) -> FrameDispatcher:
    # No chat client — every path exercised here is protocol-only.
    return FrameDispatcher(store, chat_client=None)


def _sink() -> tuple[list[dict], object]:
    events: list[dict] = []
    return events, events.append


async def _list(dispatcher: FrameDispatcher, **frame) -> dict:
    events, sink = _sink()
    await dispatcher.dispatch(json.dumps({"action": "list_conversations", **frame}), sink)
    convs = [e for e in events if e.get("type") == "immediate_response"]
    assert len(convs) == 1
    return convs[0]


async def _create(dispatcher: FrameDispatcher, **frame) -> dict:
    events, sink = _sink()
    await dispatcher.dispatch(json.dumps({"action": "create_conversation_session", "agentId": "", **frame}), sink)
    resp = [e for e in events if e.get("type") == "immediate_response"]
    assert len(resp) == 1
    return resp[0]["data"]


async def test_empty_conversations_are_filtered() -> None:
    """A conversation with no messages (every create mints one) is excluded."""
    store = InMemorySessionStore()
    d = _dispatcher(store)
    empty = await _create(d)  # no messages appended
    populated = await _create(d)
    await store.append_message(populated["conversationId"], MessageDirection.INBOUND, "hello there")

    resp = await _list(d)
    convs = resp["data"]["conversations"]
    ids = {c["conversationId"] for c in convs}
    assert populated["conversationId"] in ids
    assert empty["conversationId"] not in ids
    assert resp["message"] == "Conversations"
    (only,) = convs
    assert only["messageCount"] == 1
    assert "updatedAt" in only and only["updatedAt"]


async def test_title_is_first_inbound_preview_markdown_stripped() -> None:
    """Title = the first INBOUND message, leading markdown/control chars stripped and
    truncated to ~60 chars with an ellipsis. Outbound-first still picks the inbound."""
    store = InMemorySessionStore()
    d = _dispatcher(store)
    session = await _create(d)
    cid = session["conversationId"]
    # An outbound greeting precedes the user's first message — the title must skip it.
    await store.append_message(cid, MessageDirection.OUTBOUND, "Hi! How can I help?")
    long = "## Please help me reset my password because I forgot it entirely again today friend"
    await store.append_message(cid, MessageDirection.INBOUND, long)

    (conv,) = (await _list(d))["data"]["conversations"]
    title = conv["title"]
    # Only LEADING markdown/control chars are stripped (interior markup is preserved).
    assert not title.startswith("#")
    assert not title.startswith(" ")
    assert title.startswith("Please help me reset")
    assert len(title) <= 61  # 60 chars + the ellipsis
    assert title.endswith("…")


async def test_title_falls_back_to_conversation_name_without_inbound() -> None:
    """A conversation with only outbound messages has no inbound title source, so the
    title falls back to the conversation's name (``Session <id>``)."""
    store = InMemorySessionStore()
    d = _dispatcher(store)
    session = await _create(d)
    await store.append_message(session["conversationId"], MessageDirection.OUTBOUND, "system note")

    (conv,) = (await _list(d))["data"]["conversations"]
    assert conv["title"].startswith("Session ")


async def test_most_recent_first_ordering() -> None:
    """Conversations are returned most-recently-updated first."""
    store = InMemorySessionStore()
    d = _dispatcher(store)
    first = await _create(d)
    second = await _create(d)
    await store.append_message(first["conversationId"], MessageDirection.INBOUND, "older")
    await asyncio.sleep(0.005)
    await store.append_message(second["conversationId"], MessageDirection.INBOUND, "newer")

    convs = (await _list(d))["data"]["conversations"]
    assert [c["conversationId"] for c in convs] == [second["conversationId"], first["conversationId"]]


async def test_limit_caps_results() -> None:
    """The optional ``limit`` caps the returned list after filtering + sorting."""
    store = InMemorySessionStore()
    d = _dispatcher(store)
    for i in range(3):
        s = await _create(d)
        await store.append_message(s["conversationId"], MessageDirection.INBOUND, f"msg {i}")

    assert len((await _list(d, limit=2))["data"]["conversations"]) == 2
    assert len((await _list(d))["data"]["conversations"]) == 3  # default 50 → all


async def test_resume_binds_to_existing_conversation() -> None:
    """Creating a session with a known ``conversationId`` reuses that conversation and
    its message history (no fresh conversation minted)."""
    store = InMemorySessionStore()
    d = _dispatcher(store)
    first = await _create(d)
    cid = first["conversationId"]
    await store.append_message(cid, MessageDirection.INBOUND, "resume me")

    resumed = await _create(d, conversationId=cid, userName="Bob")
    assert resumed["conversationId"] == cid
    assert resumed["sessionId"] != first["sessionId"]
    # History is intact and not doubled — still exactly one conversation, one message.
    assert len(await store.list_conversations()) == 1
    assert len(await store.list_messages(cid, 100)) == 1
    (conv,) = (await _list(d))["data"]["conversations"]
    assert conv["conversationId"] == cid
    assert conv["messageCount"] == 1


async def test_unknown_conversation_id_falls_back_to_new() -> None:
    """An unknown ``conversationId`` mints a fresh conversation (never honors the
    caller's id blindly)."""
    store = InMemorySessionStore()
    d = _dispatcher(store)
    session = await _create(d, conversationId="does-not-exist")
    assert session["conversationId"] != "does-not-exist"
    assert await store.get_conversation("does-not-exist") is None
    assert await store.get_conversation(session["conversationId"]) is not None
