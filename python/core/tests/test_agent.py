"""Non-network unit tests for the Python core: the agentic loop, tool calling, and
knowledge injection, driven by a fake OpenAI-compatible client. Always green (no
credentials needed) — the live-gateway behavior is covered by ``test_evals.py``.
"""

from __future__ import annotations

from types import SimpleNamespace

import pytest

from smooth_operator_core import AgentOptions, FunctionTool, InMemoryKnowledge, SmoothAgent


# ── a tiny fake of the openai client surface the agent uses ──────────────────
def _msg(content=None, tool_calls=None):
    return SimpleNamespace(content=content, tool_calls=tool_calls)


def _tool_call(call_id: str, name: str, arguments: str):
    return SimpleNamespace(id=call_id, function=SimpleNamespace(name=name, arguments=arguments))


class _FakeCompletions:
    def __init__(self, scripted):
        self._scripted = list(scripted)
        self.calls: list[dict] = []

    async def create(self, **kwargs):
        self.calls.append(kwargs)
        message = self._scripted.pop(0)
        return SimpleNamespace(choices=[SimpleNamespace(message=message)])


class FakeClient:
    def __init__(self, scripted):
        self.chat = SimpleNamespace(completions=_FakeCompletions(scripted))


def test_knowledge_query_ranks_by_overlap():
    kb = InMemoryKnowledge()
    kb.ingest("The return window is 17 days from delivery.", "returns.md")
    kb.ingest("Gift wrapping costs 4.99 per item.", "wrapping.md")
    hits = kb.query("what is the return window?", top_k=1)
    assert len(hits) == 1
    assert "17 days" in hits[0].content


@pytest.mark.asyncio
async def test_text_reply_stops_after_one_call():
    client = FakeClient([_msg(content="the answer is 42")])
    agent = SmoothAgent(client, AgentOptions(instructions="be helpful"))
    result = await agent.run("what is the answer?")
    assert result.text == "the answer is 42"
    assert result.iterations == 1
    assert result.tool_calls == 0


@pytest.mark.asyncio
async def test_tool_call_then_finish():
    async def echo(args):
        return args.get("text", "")

    tool = FunctionTool(
        name="echo",
        description="Echoes input back",
        parameters={"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]},
        func=echo,
    )
    client = FakeClient(
        [
            _msg(content=None, tool_calls=[_tool_call("call-1", "echo", '{"text": "hello tools"}')]),
            _msg(content="done"),
        ]
    )
    agent = SmoothAgent(client, AgentOptions(tools=[tool]))
    result = await agent.run("use echo")
    assert result.text == "done"
    assert result.tool_calls == 1
    # The tool result was fed back as a tool-role message before the final call.
    second_call_messages = client.chat.completions.calls[1]["messages"]
    assert any(m.get("role") == "tool" and m.get("content") == "hello tools" for m in second_call_messages)


@pytest.mark.asyncio
async def test_knowledge_is_injected_into_system_prompt():
    kb = InMemoryKnowledge()
    kb.ingest("The return window is exactly 17 days from delivery.", "returns.md")
    client = FakeClient([_msg(content="17 days")])
    agent = SmoothAgent(client, AgentOptions(instructions="support agent", knowledge=kb))
    await agent.run("how many days to return?")
    system = client.chat.completions.calls[0]["messages"][0]
    assert system["role"] == "system"
    assert "17 days" in system["content"]
