"""Integration tests for per-agent config in the TurnRunner (SMOODEV-590):
prompt assembly (instructions / persona / greeting / workflow), post-turn judge
advancement, current-step persistence, and per-agent isolation via the dispatcher.
"""

from __future__ import annotations

import pytest
from smooth_operator_core import MockLlmProvider

from smooth_operator_server.agent_config import (
    AgentConfig,
    ConversationWorkflow,
    ConversationWorkflowStep,
)
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import DEFAULT_SYSTEM_PROMPT, TurnRunner


def _system_prompt(mock: MockLlmProvider) -> str:
    """The system message content the agent sent on its (first) turn."""
    messages = mock.calls[0].messages
    return next(m["content"] for m in messages if m["role"] == "system")


def _wf() -> ConversationWorkflow:
    return ConversationWorkflow(
        goal="Book the demo",
        steps=[
            ConversationWorkflowStep(id="greet", intent="Greet", criteria="greeted", next="qualify"),
            ConversationWorkflowStep(id="qualify", intent="Learn need", criteria="need known"),
        ],
    )


async def _run(mock: MockLlmProvider, store: InMemorySessionStore, agent_config: AgentConfig | None, **kw):
    runner = TurnRunner(chat_client=mock, store=store, agent_config=agent_config)
    return await runner.run(
        conversation_id=kw.get("conversation_id", "conv-1"),
        request_id="r-1",
        user_message=kw.get("user_message", "hello"),
        sink=lambda _e: None,
    )


# ── prompt assembly ──────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_no_config_uses_server_default_prompt() -> None:
    mock = MockLlmProvider()
    mock.push_text("hi")
    await _run(mock, InMemorySessionStore(), agent_config=None)
    assert _system_prompt(mock) == DEFAULT_SYSTEM_PROMPT


@pytest.mark.asyncio
async def test_agent_instructions_override_default() -> None:
    mock = MockLlmProvider()
    mock.push_text("hi")
    await _run(mock, InMemorySessionStore(), AgentConfig(instructions="You are Ada, a billing specialist."))
    prompt = _system_prompt(mock)
    assert "You are Ada, a billing specialist." in prompt
    assert DEFAULT_SYSTEM_PROMPT not in prompt


@pytest.mark.asyncio
async def test_personality_appended() -> None:
    mock = MockLlmProvider()
    mock.push_text("hi")
    await _run(mock, InMemorySessionStore(), AgentConfig(instructions="Base.", personality="dry and witty"))
    assert "dry and witty" in _system_prompt(mock)


@pytest.mark.asyncio
async def test_greeting_only_on_first_turn() -> None:
    store = InMemorySessionStore()
    config = AgentConfig(instructions="Base.", greeting="Welcome to Acme!")

    first = MockLlmProvider()
    first.push_text("Hello there")
    await _run(first, store, config)
    assert "Welcome to Acme!" in _system_prompt(first)

    # Second turn (history now exists) → no greeting section.
    second = MockLlmProvider()
    second.push_text("More help")
    await _run(second, store, config)
    assert "Welcome to Acme!" not in _system_prompt(second)


@pytest.mark.asyncio
async def test_workflow_section_rendered() -> None:
    mock = MockLlmProvider()
    mock.push_text("Hi!")  # agent turn
    mock.push_text('{"verdict": "no"}')  # judge turn
    await _run(mock, InMemorySessionStore(), AgentConfig(instructions="Base.", conversation_workflow=_wf()))
    prompt = _system_prompt(mock)
    assert "<ConversationWorkflow>" in prompt
    assert "CURRENT STEP (1/2): greet" in prompt


# ── judge advancement + current-step persistence ─────────────────────────────


@pytest.mark.asyncio
async def test_judge_yes_advances_step() -> None:
    store = InMemorySessionStore()
    config = AgentConfig(instructions="Base.", conversation_workflow=_wf())
    mock = MockLlmProvider()
    mock.push_text("Hello!")  # agent
    mock.push_text('{"verdict": "yes"}')  # judge → advance greet → qualify
    await _run(mock, store, config)
    assert await store.get_current_step_id("conv-1") == "qualify"


@pytest.mark.asyncio
async def test_judge_no_stays_on_step() -> None:
    store = InMemorySessionStore()
    config = AgentConfig(instructions="Base.", conversation_workflow=_wf())
    mock = MockLlmProvider()
    mock.push_text("Hello!")
    mock.push_text('{"verdict": "no"}')
    await _run(mock, store, config)
    # Stayed on the first step — pointer pinned to greet (matches TS: always writes current.id).
    assert await store.get_current_step_id("conv-1") == "greet"


@pytest.mark.asyncio
async def test_workflow_progresses_across_turns_and_terminates() -> None:
    store = InMemorySessionStore()
    config = AgentConfig(instructions="Base.", conversation_workflow=_wf())

    # Turn 1: greet satisfied → qualify.
    m1 = MockLlmProvider()
    m1.push_text("Hi!")
    m1.push_text('{"verdict": "yes"}')
    await _run(m1, store, config, user_message="hey")
    assert await store.get_current_step_id("conv-1") == "qualify"
    assert "CURRENT STEP (1/2): greet" in _system_prompt(m1)

    # Turn 2: qualify satisfied → terminal (no next → stays on qualify id).
    m2 = MockLlmProvider()
    m2.push_text("What do you need?")
    m2.push_text('{"verdict": "yes"}')
    await _run(m2, store, config, user_message="I need pricing")
    assert "CURRENT STEP (2/2): qualify" in _system_prompt(m2)
    assert await store.get_current_step_id("conv-1") == "qualify"


@pytest.mark.asyncio
async def test_judge_failure_does_not_break_turn() -> None:
    store = InMemorySessionStore()
    config = AgentConfig(instructions="Base.", conversation_workflow=_wf())
    mock = MockLlmProvider()
    mock.push_text("Hello!")
    mock.push_error(RuntimeError("judge gateway down"))
    result = await _run(mock, store, config)
    # Turn still succeeds; pointer stays on the current step (greet) on the judge failure.
    assert result.reply == "Hello!"
    assert await store.get_current_step_id("conv-1") == "greet"


@pytest.mark.asyncio
async def test_no_workflow_makes_no_judge_call() -> None:
    store = InMemorySessionStore()
    mock = MockLlmProvider()
    mock.push_text("Hello!")
    await _run(mock, store, AgentConfig(instructions="Base."))
    # Only the agent turn — no post-turn judge call when no workflow is configured.
    assert mock.call_count == 1


# ── per-agent isolation via the dispatcher ───────────────────────────────────


@pytest.mark.asyncio
async def test_dispatcher_isolates_config_per_agent() -> None:
    store = InMemorySessionStore()
    sess_a = await store.create_session("agent-a", None, None)
    sess_b = await store.create_session("agent-b", None, None)

    prompts: dict[str, str] = {}

    # One shared mock records every create() call; each session's turn is a single
    # agent call (no workflow), so calls[0] and calls[1] are agent A and B in order.
    mock = MockLlmProvider()
    mock.push_text("a-reply")
    mock.push_text("b-reply")

    dispatcher = FrameDispatcher(
        store,
        mock,
        agent_configs={
            "agent-a": AgentConfig(instructions="I am agent A."),
            "agent-b": AgentConfig(instructions="I am agent B."),
        },
    )

    for sess, key in ((sess_a, "a"), (sess_b, "b")):
        await dispatcher.dispatch(
            '{"action":"send_message","sessionId":"%s","message":"hi"}' % sess.session_id,
            lambda _e: None,
        )
        await dispatcher.wait_for_turns()

    prompts["a"] = next(m["content"] for m in mock.calls[0].messages if m["role"] == "system")
    prompts["b"] = next(m["content"] for m in mock.calls[1].messages if m["role"] == "system")
    assert "I am agent A." in prompts["a"]
    assert "I am agent B." in prompts["b"]
    assert "agent B" not in prompts["a"]
