"""End-to-end auth-level enforcement + per-tool config + judge-model tests.

Drives the REAL dispatcher/turn path with a scripted LLM that issues a tool call,
so gating happens exactly where it does in production (inside the engine's tool
loop, via the server's `_GatedTool` wrapper).
"""

from __future__ import annotations

import pytest
from smooth_operator_core import MockLlmProvider

from smooth_operator_server.agent_config import (
    CONFIG_ARG_KEY,
    AgentConfig,
    EnabledTool,
    SessionAuthenticator,
    StaticAgentConfigResolver,
)
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore


class SpyTool:
    """Duck-typed engine Tool that records its invocations."""

    def __init__(self, name: str, *, supports_auth: bool = False) -> None:
        self.name = name
        self.description = f"the {name} tool"
        self.parameters = {"type": "object", "properties": {}}
        self.supports_auth_requirement = supports_auth
        self.calls: list[dict] = []

    async def execute(self, arguments: dict) -> str:
        self.calls.append(arguments)
        return f"ran {self.name}"


class _AlwaysAuthed(SessionAuthenticator):
    async def is_authenticated(self, conversation_id: str) -> bool:
        return True


def _tool_results(events: list[dict]) -> list[str]:
    """The `result` strings from every tool-result stream_chunk in `events`."""
    out = []
    for e in events:
        state = e.get("data", {}).get("state") if isinstance(e, dict) else None
        result = (state or {}).get("rawResponse", {}).get("toolResult") if isinstance(state, dict) else None
        if result:
            out.append(result["result"])
    return out


async def _run_tool_turn(
    tool: SpyTool,
    config: AgentConfig,
    *,
    authenticator: SessionAuthenticator | None = None,
    args: str = "{}",
) -> list[str]:
    """Open a session for an agent with `config`, drive one send_message whose LLM
    turn calls `tool`, and return the tool-result strings emitted to the sink."""
    store = InMemorySessionStore()
    session = await store.create_session("agent-x", None, None)

    mock = MockLlmProvider()
    mock.push_tool_call("call-1", tool.name, args)  # LLM asks for the tool
    mock.push_text("all done")  # follow-up reply after the tool result

    dispatcher = FrameDispatcher(
        store,
        mock,
        tools=[tool],
        agent_config_resolver=StaticAgentConfigResolver({"agent-x": config}),
        session_authenticator=authenticator,
    )

    events: list[dict] = []
    await dispatcher.dispatch(
        '{"action":"send_message","sessionId":"%s","message":"go"}' % session.session_id,
        events.append,
    )
    await dispatcher.wait_for_turns()
    return _tool_results(events)


@pytest.mark.asyncio
async def test_admin_tool_blocked_on_public_agent() -> None:
    tool = SpyTool("crm", supports_auth=True)
    config = AgentConfig(enabled_tools=[EnabledTool("crm", auth_level="admin")], visibility="public")
    results = await _run_tool_turn(tool, config)
    assert tool.calls == []  # never executed
    assert "requires admin authentication and is not available on public-facing agents" in results[0]


@pytest.mark.asyncio
async def test_end_user_tool_blocked_when_unauthenticated() -> None:
    tool = SpyTool("crm", supports_auth=True)
    config = AgentConfig(enabled_tools=[EnabledTool("crm", auth_level="end_user")], visibility="public")
    # Default authenticator fails closed → identity required, tool NOT executed.
    results = await _run_tool_turn(tool, config)
    assert tool.calls == []
    assert "requires identity verification" in results[0]


@pytest.mark.asyncio
async def test_end_user_tool_runs_when_authenticated() -> None:
    tool = SpyTool("crm", supports_auth=True)
    config = AgentConfig(enabled_tools=[EnabledTool("crm", auth_level="end_user")], visibility="public")
    results = await _run_tool_turn(tool, config, authenticator=_AlwaysAuthed())
    assert tool.calls == [{}]  # executed
    assert results[0] == "ran crm"


@pytest.mark.asyncio
async def test_internal_agent_auto_satisfies_auth() -> None:
    tool = SpyTool("crm", supports_auth=True)
    # admin on an internal agent: auto-satisfied, no authenticator consulted.
    config = AgentConfig(enabled_tools=[EnabledTool("crm", auth_level="admin")], visibility="internal")
    results = await _run_tool_turn(tool, config)
    assert tool.calls == [{}]
    assert results[0] == "ran crm"


@pytest.mark.asyncio
async def test_auth_ignored_when_tool_does_not_support_requirement() -> None:
    # authLevel set but the tool doesn't opt in → no gating (faithful to reference).
    tool = SpyTool("crm", supports_auth=False)
    config = AgentConfig(enabled_tools=[EnabledTool("crm", auth_level="admin")], visibility="public")
    results = await _run_tool_turn(tool, config)
    assert tool.calls == [{}]
    assert results[0] == "ran crm"


@pytest.mark.asyncio
async def test_per_tool_config_delivered_to_tool() -> None:
    tool = SpyTool("crm")
    config = AgentConfig(enabled_tools=[EnabledTool("crm", config={"base_url": "https://crm.example"})])
    await _run_tool_turn(tool, config)
    assert tool.calls[0][CONFIG_ARG_KEY] == {"base_url": "https://crm.example"}


@pytest.mark.asyncio
async def test_no_config_leaves_args_clean() -> None:
    tool = SpyTool("crm")
    config = AgentConfig(enabled_tools=[EnabledTool("crm")])  # no per-tool config
    await _run_tool_turn(tool, config)
    assert CONFIG_ARG_KEY not in tool.calls[0]


@pytest.mark.asyncio
async def test_judge_model_option_used() -> None:
    from smooth_operator_server.agent_config import ConversationWorkflow, ConversationWorkflowStep

    store = InMemorySessionStore()
    session = await store.create_session("agent-x", None, None)
    workflow = ConversationWorkflow(
        goal="g",
        steps=[
            ConversationWorkflowStep(id="s1", intent="i", criteria="c", next="s2"),
            ConversationWorkflowStep(id="s2", intent="i2", criteria="c2"),
        ],
    )
    config = AgentConfig(instructions="Base.", conversation_workflow=workflow)

    mock = MockLlmProvider()
    mock.push_text("hello")  # agent turn
    mock.push_text('{"verdict": "yes"}')  # judge turn

    dispatcher = FrameDispatcher(
        store,
        mock,
        agent_config_resolver=StaticAgentConfigResolver({"agent-x": config}),
        judge_model="custom-judge-model",
    )
    await dispatcher.dispatch(
        '{"action":"send_message","sessionId":"%s","message":"hi"}' % session.session_id,
        lambda _e: None,
    )
    await dispatcher.wait_for_turns()

    # The judge call is the non-streaming create; assert it used the configured model
    # and that the workflow advanced (proof the judge ran).
    judge_models = [c.kwargs.get("model") for c in mock.calls if c.kwargs.get("model") == "custom-judge-model"]
    assert judge_models == ["custom-judge-model"]
    assert await store.get_current_step_id(session.conversation_id) == "s2"
