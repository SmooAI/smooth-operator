"""``supports`` survives a reconnect (th-13df6d).

``supports`` is the client render-capability list declared on
``create_conversation_session``; it gates the entire Rich Interactions framework (a kind
whose capability is declared parks the turn and emits ``interaction_required``, one
without it degrades to a conversational fallback).

A RECONNECT is a resume: the client re-opens the socket and re-issues
``create_conversation_session`` with the same ``conversationId``, which mints a NEW
session on a NEW :class:`FrameDispatcher`. While the declared list lived in a
per-connection dict on the dispatcher, every network blip / backgrounding / deploy
silently dropped it and the shipped feature degraded with nothing on the wire to notice.
It now rides the conversation, the same durability the workflow-step pointer has.

The assertions go through the thing the declaration is FOR: the ``capabilities`` the
dispatcher hands the turn. A spy :class:`TurnRunner` captures them, so a regression that
persists the list but forgets to read it back still fails.
"""

from __future__ import annotations

import json
from typing import Any

import pytest

from smooth_operator_server import dispatcher as dispatcher_module
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import TurnResult

_AGENT = "11111111-1111-1111-1111-111111111111"


class _SpyRunner:
    """Stands in for :class:`TurnRunner`, recording the capabilities it was built with
    and completing the turn immediately (no LLM, no streaming)."""

    last_capabilities: list[str] | None = None

    def __init__(self, *_args: Any, capabilities: list[str] | None = None, **_kwargs: Any) -> None:
        _SpyRunner.last_capabilities = capabilities

    async def run(self, *_args: Any, **_kwargs: Any) -> TurnResult:
        return TurnResult(reply="ok", message_id="m-1")


async def _dispatch(dispatcher: FrameDispatcher, frame: dict) -> list[dict]:
    """Dispatch one frame, collecting every event emitted to the sink."""
    events: list[dict] = []
    await dispatcher.dispatch(json.dumps(frame), events.append)
    return events


async def _create(dispatcher: FrameDispatcher, request_id: str, **extra: Any) -> tuple[str, str]:
    """``create_conversation_session`` → ``(sessionId, conversationId)``."""
    events = await _dispatch(
        dispatcher, {"action": "create_conversation_session", "requestId": request_id, "agentId": _AGENT, **extra}
    )
    data = events[0]["data"]
    return data["sessionId"], data["conversationId"]


async def _turn_capabilities(dispatcher: FrameDispatcher, session_id: str) -> list[str] | None:
    """The capabilities a turn on this session is actually handed."""
    _SpyRunner.last_capabilities = None
    await _dispatch(
        dispatcher, {"action": "send_message", "requestId": "r-msg", "sessionId": session_id, "message": "hi"}
    )
    # The turn runs as a background task; capabilities are captured when the runner is
    # constructed (before the spawn), but await the task so it never outlives the test.
    for task in list(dispatcher._turn_tasks):  # noqa: SLF001 — no public handle on the in-flight turn
        await task
    return _SpyRunner.last_capabilities


@pytest.fixture(autouse=True)
def _spy_runner(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(dispatcher_module, "TurnRunner", _SpyRunner)


@pytest.mark.asyncio
async def test_reconnect_resuming_a_conversation_keeps_the_declared_capabilities() -> None:
    store = InMemorySessionStore()

    # First connection: declares the capability.
    first = FrameDispatcher(store, object())
    session_id, conversation_id = await _create(first, "r-conn-1", supports=["identity_form"])
    assert await _turn_capabilities(first, session_id) == ["identity_form"]

    # Reconnect: a SECOND, fresh dispatcher over the same store (that is what a new
    # socket is), same conversation, `supports` omitted — exactly what a widget resuming
    # from its stored conversationId sends. This is where the feature went dark.
    second = FrameDispatcher(store, object())
    resumed_id, resumed_conversation = await _create(second, "r-conn-2", conversationId=conversation_id)
    assert resumed_conversation == conversation_id, "the reconnect resumed the same conversation"
    assert resumed_id != session_id, "a reconnect mints a NEW session"
    assert await _turn_capabilities(second, resumed_id) == ["identity_form"], (
        "a reconnect that omits 'supports' inherits the conversation's declared capabilities"
    )

    # A resume that DECLARES wins over the inherited set, in both directions — so a
    # text-only client resuming a rich conversation opts out with `[]` rather than being
    # handed cards it cannot render.
    third = FrameDispatcher(store, object())
    text_only_id, _ = await _create(third, "r-conn-3", conversationId=conversation_id, supports=[])
    assert not await _turn_capabilities(third, text_only_id), (
        "an explicit empty 'supports' declares text-only and never inherits"
    )

    # ...and that opt-out is itself durable: the NEXT reconnect omitting the key must not
    # resurrect the capability from a stale record.
    fourth = FrameDispatcher(store, object())
    after_id, _ = await _create(fourth, "r-conn-4", conversationId=conversation_id)
    assert not await _turn_capabilities(fourth, after_id), "the text-only declaration replaced the durable record"


@pytest.mark.asyncio
async def test_fresh_conversation_without_supports_is_text_only() -> None:
    """The inherit rule keys on the CONVERSATION, so a brand-new one still starts empty —
    nothing to inherit means unchanged behavior, not someone else's capabilities."""
    store = InMemorySessionStore()
    dispatcher = FrameDispatcher(store, object())

    rich_id, _ = await _create(dispatcher, "r-1", supports=["identity_form"])
    assert await _turn_capabilities(dispatcher, rich_id) == ["identity_form"]

    fresh_id, _ = await _create(dispatcher, "r-2")
    assert not await _turn_capabilities(dispatcher, fresh_id)
