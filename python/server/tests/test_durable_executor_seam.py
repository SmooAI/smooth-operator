"""The server's durable-executor selection seam (ADR-030, item Q).

``TurnRunner`` runs each turn through an ``AgentExecutor`` rather than calling
``SmoothAgent.run_stream`` directly, so a durable backend can be selected in one
place — mirroring the Rust server's ``turn_executor`` (``runner.rs``). The seam is
dependency-**injected**: the durable backend is passed in as an opaque
``AgentExecutor``, so the server needs no hard dependency on the Temporal package.

These tests cover the selection logic (``durable_requested`` parse +
``select_turn_executor``) and prove end to end that an injected executor is used
when — and only when — ``SMOOTH_AGENT_DURABLE_EXECUTOR`` opts in.
"""

from __future__ import annotations

from typing import Any

import pytest
from smooth_operator_core import InProcessExecutor, MockLlmProvider

from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import (
    DURABLE_EXECUTOR_ENV,
    TurnRunner,
    durable_requested,
    select_turn_executor,
)


class SpyExecutor:
    """A fake durable ``AgentExecutor`` that records it was used, then delegates to
    the real agent so the turn still completes."""

    def __init__(self) -> None:
        self.used = False

    async def execute(self, agent: Any, message: str, history: Any = None, thread: Any = None) -> Any:
        self.used = True
        return await agent.run(message, history, thread)

    def execute_streaming(self, agent: Any, message: str, history: Any = None, thread: Any = None) -> Any:
        self.used = True
        return agent.run_stream(message, history, thread)


# ── durable_requested: opt-in parse (mirrors the Rust table) ─────────────────


@pytest.mark.parametrize("value", ["1", "true", "TRUE", " on ", "yes"])
def test_durable_requested_opts_in(value: str):
    assert durable_requested(value) is True


@pytest.mark.parametrize("value", ["", " ", "0", "false", "off", "no", "maybe", None])
def test_durable_requested_stays_off(value: str | None):
    assert durable_requested(value) is False


# ── select_turn_executor: injection + env gating ─────────────────────────────


def test_injected_executor_used_when_env_opts_in():
    spy = SpyExecutor()
    assert select_turn_executor(spy, env_value="1") is spy


def test_injected_executor_ignored_when_env_off():
    """Env off ⇒ in-process, even with a durable executor injected — an injected
    backend never silently takes over a deployment that didn't ask."""
    spy = SpyExecutor()
    selected = select_turn_executor(spy, env_value="0")
    assert selected is not spy
    assert isinstance(selected, InProcessExecutor)


def test_env_on_without_injection_falls_back_to_in_process():
    selected = select_turn_executor(None, env_value="true")
    assert isinstance(selected, InProcessExecutor)


def test_each_fallback_builds_its_own_in_process_executor():
    a = select_turn_executor(None, env_value=None)
    b = select_turn_executor(None, env_value=None)
    assert isinstance(a, InProcessExecutor)
    assert a is not b


# ── TurnRunner wires the seam, and the turn actually routes through it ────────


async def test_turn_runs_through_injected_executor_when_env_opts_in(monkeypatch):
    monkeypatch.setenv(DURABLE_EXECUTOR_ENV, "1")
    spy = SpyExecutor()
    mock = MockLlmProvider().push_text("durable reply")

    runner = TurnRunner(chat_client=mock, store=InMemorySessionStore(), executor=spy)
    result = await runner.run(conversation_id="c1", request_id="r1", user_message="go", sink=lambda _e: None)

    assert spy.used is True
    assert result.reply == "durable reply"


async def test_turn_runs_in_process_when_env_off(monkeypatch):
    monkeypatch.delenv(DURABLE_EXECUTOR_ENV, raising=False)
    spy = SpyExecutor()
    mock = MockLlmProvider().push_text("in-process reply")

    runner = TurnRunner(chat_client=mock, store=InMemorySessionStore(), executor=spy)
    result = await runner.run(conversation_id="c2", request_id="r2", user_message="go", sink=lambda _e: None)

    # The injected executor was NOT used — the in-process path ran instead.
    assert spy.used is False
    assert isinstance(runner._executor, InProcessExecutor)
    assert result.reply == "in-process reply"
