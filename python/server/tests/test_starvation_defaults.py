"""The server's raised anti-starvation turn sizing (EPIC th-1cc9fa).

The old chat-widget defaults (max_tokens=512, max_iterations=6) STARVE reasoning
models — they exhaust the token budget on reasoning and return empty, or run out of
loop iterations mid-tool-use. Raised to 8192 / 20, mirroring the Rust server's
``DEFAULT_MAX_TOKENS`` / ``DEFAULT_MAX_ITERATIONS``. The engine still clamps
``max_tokens`` DOWN to each model's real output ceiling, so raising it is safe.
"""

from __future__ import annotations

import pytest
from smooth_operator_core import MockLlmProvider

from smooth_operator_server import turn_runner as tr
from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import (
    DEFAULT_MAX_ITERATIONS,
    DEFAULT_MAX_TOKENS,
    TurnRunner,
)


def test_defaults_are_raised():
    assert DEFAULT_MAX_TOKENS == 8192
    assert DEFAULT_MAX_ITERATIONS == 20


@pytest.mark.asyncio
async def test_turn_sends_raised_max_tokens():
    mock = MockLlmProvider()
    mock.push_text("hello")
    runner = TurnRunner(chat_client=mock, store=InMemorySessionStore())
    await runner.run(conversation_id="c1", request_id="r1", user_message="hi", sink=lambda _e: None)
    assert mock.last_call is not None
    # No gateway key in the test env ⇒ no ceiling ⇒ the raised default is sent as-is.
    assert mock.last_call.kwargs["max_tokens"] == DEFAULT_MAX_TOKENS


@pytest.mark.asyncio
async def test_iteration_cap_is_raised():
    # Script MORE tool-calls than the cap; the loop must stop at DEFAULT_MAX_ITERATIONS
    # (proving the cap is 20, not the engine's old 6/8). Each references an unknown
    # tool, so every iteration makes exactly one model call then loops.
    mock = MockLlmProvider()
    for i in range(DEFAULT_MAX_ITERATIONS + 5):
        mock.push_tool_call(f"call-{i}", "nonexistent_tool", "{}")
    runner = TurnRunner(chat_client=mock, store=InMemorySessionStore())
    await runner.run(conversation_id="c2", request_id="r2", user_message="loop", sink=lambda _e: None)
    assert mock.call_count == DEFAULT_MAX_ITERATIONS


@pytest.mark.skipif(
    not tr._CORE_SUPPORTS_CEILING,
    reason="installed smooth-operator-core predates the model_max_output clamp (release-ordering: core ships first)",
)
@pytest.mark.asyncio
async def test_ceiling_clamps_raised_max_tokens_end_to_end(monkeypatch):
    # Forward-compat: once a core WITH the clamp is installed, a resolved ceiling
    # below the raised 8192 default is threaded into AgentOptions and the engine
    # sends the clamped value. Skipped on the pinned (pre-clamp) core.
    async def _fake_ceiling(model: str) -> int:
        return 4096

    monkeypatch.setattr(tr, "model_output_ceiling", _fake_ceiling)
    mock = MockLlmProvider()
    mock.push_text("clamped")
    runner = TurnRunner(chat_client=mock, store=InMemorySessionStore())
    await runner.run(conversation_id="c3", request_id="r3", user_message="hi", sink=lambda _e: None)
    assert mock.last_call is not None
    assert mock.last_call.kwargs["max_tokens"] == 4096
