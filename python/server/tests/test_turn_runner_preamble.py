"""Unit tests for the optional fast-model preamble (``SMOOTH_AGENT_PREAMBLE_MODEL``).

This feature is mostly defined by what must NOT happen, so the tests are mostly
negatives: no extra LLM call when it's off, nothing emitted once the real answer
has begun, no error event when it fails, and never a trace of it in the persisted
reply. Every ordering assertion is made DETERMINISTIC by gating the fake client's
responses on ``asyncio.Event``s the sink flips — no sleeps, no racing.

Mirrors the Rust reference server's preamble tests. Pearl th-ce3888.
"""

from __future__ import annotations

import asyncio
from types import SimpleNamespace
from typing import Any

import pytest
from smooth_operator_core import MockLlmProvider

from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import (
    PREAMBLE_MAX_TOKENS,
    PREAMBLE_SYSTEM_PROMPT,
    TurnRunner,
    preamble_model,
)

ENV_VAR = "SMOOTH_AGENT_PREAMBLE_MODEL"
FAST_MODEL = "groq-gpt-oss-20b"
AGENT_TEXT = "Your return window is 30 days."
PREAMBLE_TEXT = "Let me pull up your return policy."
#: Bounds a gate that never opens, so a broken ordering fails the test instead of
#: hanging the suite.
GATE_TIMEOUT = 5.0


def _text_response(content: str) -> SimpleNamespace:
    """An OpenAI-shaped chat completion carrying a single text choice."""
    return SimpleNamespace(choices=[SimpleNamespace(message=SimpleNamespace(content=content))])


class _FakeClient:
    """A chat client that dispatches by model: calls for ``FAST_MODEL`` are the
    preamble (recorded, and optionally gated / made to fail), everything else is the
    real agent turn, delegated to a scripted :class:`MockLlmProvider`.

    ``preamble_gate`` / ``answer_gate`` are ``asyncio.Event``s the respective call
    awaits before returning — that is how the tests pin down the ordering of two
    genuinely concurrent tasks without sleeping."""

    def __init__(
        self,
        *,
        agent_text: str = AGENT_TEXT,
        preamble_text: str = PREAMBLE_TEXT,
        preamble_gate: asyncio.Event | None = None,
        answer_gate: asyncio.Event | None = None,
        preamble_error: Exception | None = None,
    ) -> None:
        self._inner = MockLlmProvider()
        self._inner.push_text(agent_text)
        self._preamble_text = preamble_text
        self._preamble_error = preamble_error
        #: Gates (public so a test can point one at :attr:`preamble_started`): the
        #: matching call blocks on these before returning.
        self.preamble_gate = preamble_gate
        self.answer_gate = answer_gate
        #: Set the moment the preamble call lands — lets a test order the agent's
        #: answer strictly AFTER the preamble is in flight.
        self.preamble_started = asyncio.Event()
        #: Every kwargs dict the preamble model was called with (empty ⇒ never called).
        self.preamble_calls: list[dict[str, Any]] = []
        self.chat = SimpleNamespace(completions=SimpleNamespace(create=self._create))

    async def _create(self, **kwargs: Any) -> Any:
        if kwargs.get("model") == FAST_MODEL:
            self.preamble_calls.append(kwargs)
            self.preamble_started.set()
            if self.preamble_gate is not None:
                await asyncio.wait_for(self.preamble_gate.wait(), GATE_TIMEOUT)
            if self._preamble_error is not None:
                raise self._preamble_error
            return _text_response(self._preamble_text)
        if self.answer_gate is not None:
            await asyncio.wait_for(self.answer_gate.wait(), GATE_TIMEOUT)
        return await self._inner.chat.completions.create(**kwargs)


async def _run(client: _FakeClient, sink: Any, store: InMemorySessionStore | None = None):
    store = store or InMemorySessionStore()
    runner = TurnRunner(chat_client=client, store=store)
    result = await runner.run(
        conversation_id="conv-1",
        request_id="r-1",
        user_message="what is the return policy?",
        sink=sink,
    )
    return result, store


def _of_type(events: list[dict[str, Any]], type_: str) -> list[dict[str, Any]]:
    return [event for event in events if event.get("type") == type_]


# ── the env contract ─────────────────────────────────────────────────────────


def test_preamble_model_off_unless_set(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv(ENV_VAR, raising=False)
    assert preamble_model() is None
    for blank in ("", "   ", "\t\n"):
        monkeypatch.setenv(ENV_VAR, blank)
        assert preamble_model() is None
    monkeypatch.setenv(ENV_VAR, f"  {FAST_MODEL}  ")
    assert preamble_model() == FAST_MODEL


@pytest.mark.asyncio
async def test_unset_emits_nothing_and_never_calls_the_model(monkeypatch: pytest.MonkeyPatch) -> None:
    """Off by default: no preamble event AND no extra LLM call at all."""
    monkeypatch.delenv(ENV_VAR, raising=False)
    events: list[dict[str, Any]] = []
    client = _FakeClient()

    result, _ = await _run(client, events.append)

    assert _of_type(events, "stream_preamble") == []
    assert client.preamble_calls == []
    assert result.reply == AGENT_TEXT


# ── the happy path ───────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_set_emits_the_documented_wire_shape(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(ENV_VAR, FAST_MODEL)
    events: list[dict[str, Any]] = []
    # Deterministic ordering: the agent's answer cannot resolve until the preamble
    # event has actually been emitted, so this test never races the guard.
    preamble_emitted = asyncio.Event()

    def sink(event: dict[str, Any]) -> None:
        events.append(event)
        if event["type"] == "stream_preamble":
            preamble_emitted.set()

    client = _FakeClient(answer_gate=preamble_emitted)
    result, store = await _run(client, sink)

    preambles = _of_type(events, "stream_preamble")
    assert len(preambles) == 1
    assert preambles[0] == {
        "type": "stream_preamble",
        "requestId": "r-1",
        "token": PREAMBLE_TEXT,
        "data": {"requestId": "r-1", "token": PREAMBLE_TEXT},
        "timestamp": preambles[0]["timestamp"],
    }
    assert isinstance(preambles[0]["timestamp"], int)
    # The turn itself is untouched.
    assert result.reply == AGENT_TEXT
    assert _of_type(events, "error") == []
    # …and the preamble is ephemeral: not in the reply, not persisted.
    assert PREAMBLE_TEXT not in result.reply
    messages = await store.list_messages("conv-1", 50)
    assert all(PREAMBLE_TEXT not in message.text for message in messages)


@pytest.mark.asyncio
async def test_preamble_uses_the_fast_model_and_a_tight_token_cap(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(ENV_VAR, FAST_MODEL)
    preamble_emitted = asyncio.Event()

    def sink(event: dict[str, Any]) -> None:
        if event["type"] == "stream_preamble":
            preamble_emitted.set()

    client = _FakeClient(answer_gate=preamble_emitted)
    await _run(client, sink)

    assert len(client.preamble_calls) == 1
    call = client.preamble_calls[0]
    assert call["model"] == FAST_MODEL
    assert call["max_tokens"] == PREAMBLE_MAX_TOKENS == 64
    # System prompt verbatim; the user's message is the ONLY user-role content, and
    # no tool results are threaded in.
    assert call["messages"] == [
        {"role": "system", "content": PREAMBLE_SYSTEM_PROMPT},
        {"role": "user", "content": "what is the return policy?"},
    ]


# ── the race guard ───────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_slow_preamble_is_dropped_once_the_answer_started(monkeypatch: pytest.MonkeyPatch) -> None:
    """The subtle one. The preamble cannot resolve until a real answer token has
    been emitted (the sink opens its gate), so this deterministically exercises the
    "answer already started" branch — a late preamble must emit nothing."""
    monkeypatch.setenv(ENV_VAR, FAST_MODEL)
    events: list[dict[str, Any]] = []
    answer_started = asyncio.Event()

    def sink(event: dict[str, Any]) -> None:
        events.append(event)
        if event["type"] == "stream_token":
            answer_started.set()

    client = _FakeClient(preamble_gate=answer_started)
    # …and symmetrically, the agent's answer waits until the preamble is in flight,
    # so the two orderings are pinned end to end: preamble starts → answer streams →
    # preamble resolves. No sleeps involved.
    client.answer_gate = client.preamble_started
    result, _ = await _run(client, sink)

    # The preamble model WAS called (it was on) but its late answer was suppressed.
    assert len(client.preamble_calls) == 1
    assert _of_type(events, "stream_preamble") == []
    assert _of_type(events, "stream_token") != []
    assert result.reply == AGENT_TEXT


# ── best-effort failure ──────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_preamble_failure_never_surfaces_or_fails_the_turn(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(ENV_VAR, FAST_MODEL)
    events: list[dict[str, Any]] = []
    client = _FakeClient(preamble_error=RuntimeError("gateway exploded"))
    # Pin the ordering so the failure is genuinely exercised: the answer waits until
    # the preamble call has landed, so this can't pass vacuously.
    client.answer_gate = client.preamble_started

    result, store = await _run(client, events.append)

    assert len(client.preamble_calls) == 1
    assert result.reply == AGENT_TEXT
    assert _of_type(events, "stream_preamble") == []
    assert _of_type(events, "error") == []
    messages = await store.list_messages("conv-1", 50)
    assert [message.text for message in messages] == ["what is the return policy?", AGENT_TEXT]


@pytest.mark.asyncio
async def test_hanging_preamble_does_not_delay_or_leak(monkeypatch: pytest.MonkeyPatch) -> None:
    """A preamble that never resolves must not gate the turn, and must be reaped at
    turn end (no pending task left behind)."""
    monkeypatch.setenv(ENV_VAR, FAST_MODEL)
    events: list[dict[str, Any]] = []
    never = asyncio.Event()  # deliberately never set
    client = _FakeClient(preamble_gate=never)
    client.answer_gate = client.preamble_started

    before = len(asyncio.all_tasks())
    result, _ = await _run(client, events.append)

    assert len(client.preamble_calls) == 1
    assert result.reply == AGENT_TEXT
    assert _of_type(events, "stream_preamble") == []
    assert _of_type(events, "error") == []
    # The still-blocked preamble task was cancelled and reaped at turn end.
    assert len(asyncio.all_tasks()) == before


@pytest.mark.asyncio
async def test_blank_preamble_text_emits_nothing(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(ENV_VAR, FAST_MODEL)
    events: list[dict[str, Any]] = []
    client = _FakeClient(preamble_text="   ")
    # The preamble runs to completion BEFORE the answer streams, so the blank-text
    # branch is what suppresses the event here — not the race guard.
    client.answer_gate = client.preamble_started

    await _run(client, events.append)

    assert len(client.preamble_calls) == 1
    assert _of_type(events, "stream_preamble") == []
