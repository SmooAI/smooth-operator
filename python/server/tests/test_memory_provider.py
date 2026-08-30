"""Durable auto-recall parity (th-ebe27d / Rust PR #330 — the
``StorageAdapter::memory_for_access`` seam, tested in
``rust/smooth-operator-server/tests/injection_seams.rs``).

The engine already knew how to recall; what was missing on this server was the host's way
to say WHICH store, so every turn ran without auto-recall no matter what the deployment had.
Tests are named after their Rust counterparts so a parity gap stays visible.

The recall block's header text is deliberately NOT asserted: the five cores currently inject
three different strings for it (th-ffaeae). The assertion is on the recalled CONTENT reaching
the model, which is the behavior the seam exists for.
"""

from __future__ import annotations

import json

import pytest
from smooth_operator_core import InMemoryMemory, MockLlmProvider

from smooth_operator_server.agent_config import StaticAgentConfigResolver
from smooth_operator_server.auth import AccessContext
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.memory import StaticMemoryProvider
from smooth_operator_server.session_store import InMemorySessionStore

RECALLED = "always add shows to the smoo-hub watchlist"


def _all_content_seen(chat: MockLlmProvider) -> str:
    """Everything the model was sent this turn, flattened — the surface a recalled memory
    must show up in."""
    return json.dumps([c.messages for c in chat.calls])


async def _run_turn(provider, message: str) -> MockLlmProvider:
    store = InMemorySessionStore()
    session = await store.create_session("agent-x", None, None)
    chat = MockLlmProvider()
    chat.push_text("ok")
    dispatcher = FrameDispatcher(
        store,
        chat,
        tools=[],
        agent_config_resolver=StaticAgentConfigResolver({}),
        memory_provider=provider,
    )
    await dispatcher.dispatch(
        json.dumps({"action": "send_message", "sessionId": session.session_id, "message": message}),
        lambda _e: None,
    )
    await dispatcher.wait_for_turns()
    return chat


def _memory_with_entry() -> InMemoryMemory:
    memory = InMemoryMemory()
    memory.remember(RECALLED)
    return memory


# ── rust: no_memory_means_no_recall_injection ────────────────────────────────


@pytest.mark.asyncio
async def test_no_memory_means_no_recall_injection() -> None:
    """Default: no provider ⇒ no auto-recall. Guards against the seam injecting when absent —
    an unopted deployment's turn must be byte-for-byte what it was before."""
    chat = await _run_turn(None, "add shows to my watchlist")
    assert RECALLED not in _all_content_seen(chat)


@pytest.mark.asyncio
async def test_provider_returning_none_means_no_recall_injection() -> None:
    """A provider that returns None for this caller is the same as no provider — the seam must
    not fabricate a store just because one was installed."""
    chat = await _run_turn(StaticMemoryProvider(None), "add shows to my watchlist")
    assert RECALLED not in _all_content_seen(chat)


# ── rust: attached_memory_is_auto_recalled_into_the_turn ─────────────────────


@pytest.mark.asyncio
async def test_attached_memory_is_auto_recalled_into_the_turn() -> None:
    """With a store attached the engine recalls the entries relevant to the user's message and
    injects them into the turn — the seam that lights up Big Smooth's durable auto-recall."""
    # The message shares "add", "shows", "watchlist" with the stored entry, so the engine's
    # word-overlap recall surfaces it.
    chat = await _run_turn(StaticMemoryProvider(_memory_with_entry()), "add shows to my watchlist")
    assert RECALLED in _all_content_seen(chat)


@pytest.mark.asyncio
async def test_irrelevant_message_recalls_nothing() -> None:
    """An unrelated message recalls nothing: the seam is relevance-gated by the engine, not a
    blanket dump of every stored memory into every turn. The message shares NO token with the
    entry — the bundled lexical scorer counts raw token overlap with no stopword filter, so a
    single shared "the" would be enough to score a hit."""
    chat = await _run_turn(StaticMemoryProvider(_memory_with_entry()), "explain quantum entanglement")
    assert RECALLED not in _all_content_seen(chat)


@pytest.mark.asyncio
async def test_provider_sees_the_callers_access() -> None:
    """The seam is access-scoped (mirroring knowledge) so a multi-tenant host can bind memory to
    the requester — the argument must actually reach the provider."""
    seen: list[AccessContext] = []

    class _Recording:
        def memory_for_access(self, access: AccessContext):
            seen.append(access)
            return None

    await _run_turn(_Recording(), "hello")
    assert len(seen) == 1
