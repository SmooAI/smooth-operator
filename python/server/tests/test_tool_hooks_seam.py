"""The server's tool-hook seam: ``TurnRunner(tool_hooks=[...])`` installs
``ToolHook``s on every turn's engine (the Python sibling of the Rust server's
``tools.add_hook(...)`` in ``runner.rs``).

Drives a real ``TurnRunner`` turn on ``MockLlmProvider`` with a scripted tool call,
so the hook fires exactly where it does in production — inside the engine's tool
loop. Skipped on a core predating the hook seam (release-ordering: core ships first),
mirroring the ceiling-clamp test in ``test_starvation_defaults``.
"""

from __future__ import annotations

import pytest
from smooth_operator_core import MockLlmProvider

from smooth_operator_server import turn_runner as tr
from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import TurnRunner


def test_core_support_is_feature_detected():
    # A plain bool — either the pinned core has the field or it doesn't. This asserts
    # the detection wire itself, and runs regardless of which core is installed.
    assert isinstance(tr._CORE_SUPPORTS_HOOKS, bool)


class _SpyTool:
    """Duck-typed engine Tool that returns a fixed string."""

    def __init__(self, name: str, output: str) -> None:
        self.name = name
        self.description = f"the {name} tool"
        self.parameters = {"type": "object", "properties": {}}
        self._output = output

    async def execute(self, arguments: dict) -> str:
        return self._output


@pytest.mark.skipif(
    not tr._CORE_SUPPORTS_HOOKS,
    reason="installed smooth-operator-core predates the tool_hooks seam (release-ordering: core ships first)",
)
@pytest.mark.asyncio
async def test_tool_hooks_fire_and_redact_end_to_end():
    from smooth_operator_core import ToolCall, ToolHook, ToolResult

    class SpyRedactHook(ToolHook):
        def __init__(self) -> None:
            self.pre: list[str] = []
            self.post: list[str] = []

        async def pre_call(self, call: ToolCall) -> None:
            self.pre.append(call.name)

        async def post_call(self, call: ToolCall, result: ToolResult) -> None:
            self.post.append(result.content)
            result.content = result.content.replace("secret", "[REDACTED]")

    spy = SpyRedactHook()
    tool = _SpyTool("lookup", "the secret value")

    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "lookup", "{}")  # the LLM asks for the tool
    mock.push_text("all done")  # follow-up reply after the tool result

    runner = TurnRunner(
        chat_client=mock,
        store=InMemorySessionStore(),
        tools=[tool],
        tool_hooks=[spy],
    )
    await runner.run(conversation_id="c1", request_id="r1", user_message="go", sink=lambda _e: None)

    # The hook saw the call on both sides of execution.
    assert spy.pre == ["lookup"]
    assert spy.post == ["the secret value"]
    # And its redaction reached the model: the follow-up call carries the scrubbed
    # tool result, not the raw one.
    tool_messages = [m for m in mock.last_call.messages if m.get("role") == "tool"]
    assert tool_messages[-1]["content"] == "the [REDACTED] value"


@pytest.mark.asyncio
async def test_no_hooks_is_default_and_unchanged():
    # The default (no tool_hooks) threads nothing into AgentOptions — the turn runs
    # exactly as before. Works on any core (nothing hook-specific is passed).
    tool = _SpyTool("lookup", "raw output")
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "lookup", "{}")
    mock.push_text("done")

    runner = TurnRunner(chat_client=mock, store=InMemorySessionStore(), tools=[tool])
    result = await runner.run(conversation_id="c2", request_id="r2", user_message="go", sink=lambda _e: None)

    assert result.reply == "done"
    tool_messages = [m for m in mock.last_call.messages if m.get("role") == "tool"]
    assert tool_messages[-1]["content"] == "raw output"
