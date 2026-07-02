"""Unit tests for the conversation-workflow step helpers + post-turn judge
(SMOODEV-590). Mirrors the monorepo's workflow.ts / workflow-judge.ts tests.
"""

from __future__ import annotations

import pytest
from smooth_operator_core import MockLlmProvider

from smooth_operator_server.agent_config import ConversationWorkflow, ConversationWorkflowStep
from smooth_operator_server.workflow import (
    judge_workflow_step,
    next_step,
    render_workflow_prompt_section,
    resolve_current_step,
)


def _wf() -> ConversationWorkflow:
    return ConversationWorkflow(
        goal="Book the demo",
        steps=[
            ConversationWorkflowStep(id="greet", intent="Greet the caller", criteria="greeted", next="qualify"),
            ConversationWorkflowStep(id="qualify", intent="Learn the need", criteria="need known"),
            ConversationWorkflowStep(id="book", intent="Book a time", criteria="time booked"),
        ],
    )


# ── resolve_current_step ─────────────────────────────────────────────────────


def test_resolve_current_step_defaults_to_first() -> None:
    assert resolve_current_step(_wf(), None).id == "greet"
    assert resolve_current_step(_wf(), "").id == "greet"


def test_resolve_current_step_matches_by_id() -> None:
    assert resolve_current_step(_wf(), "qualify").id == "qualify"


def test_resolve_current_step_unknown_id_falls_back_to_first() -> None:
    assert resolve_current_step(_wf(), "does-not-exist").id == "greet"


def test_resolve_current_step_none_workflow() -> None:
    assert resolve_current_step(None, "x") is None
    assert resolve_current_step(ConversationWorkflow(goal="g", steps=[]), None) is None


# ── next_step ────────────────────────────────────────────────────────────────


def test_next_step_explicit_next() -> None:
    wf = _wf()
    assert next_step(wf, wf.steps[0]).id == "qualify"  # greet → explicit next=qualify


def test_next_step_sequential_when_no_next() -> None:
    wf = _wf()
    assert next_step(wf, wf.steps[1]).id == "book"  # qualify has no next → sequential


def test_next_step_terminal_is_none() -> None:
    wf = _wf()
    assert next_step(wf, wf.steps[2]) is None  # book is last


def test_next_step_unknown_next_falls_through_to_sequential() -> None:
    step = ConversationWorkflowStep(id="greet", intent="i", criteria="c", next="ghost")
    wf = ConversationWorkflow(goal="g", steps=[step, ConversationWorkflowStep(id="two", intent="i", criteria="c")])
    assert next_step(wf, wf.steps[0]).id == "two"


# ── render_workflow_prompt_section ───────────────────────────────────────────


def test_render_includes_goal_step_and_position() -> None:
    section = render_workflow_prompt_section(_wf(), "qualify")
    assert "<ConversationWorkflow>" in section
    assert "GOAL: Book the demo" in section
    assert "CURRENT STEP (2/3): qualify" in section
    assert "INTENT: Learn the need" in section
    assert "CRITERIA: need known" in section
    assert section.endswith("</ConversationWorkflow>")


def test_render_defaults_to_first_step() -> None:
    assert "CURRENT STEP (1/3): greet" in render_workflow_prompt_section(_wf(), None)


def test_render_empty_when_no_workflow() -> None:
    assert render_workflow_prompt_section(None, "x") == ""


# ── judge_workflow_step ──────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_judge_yes_verdict() -> None:
    wf = _wf()
    mock = MockLlmProvider()
    mock.push_text('{"verdict": "yes", "reason": "greeted the caller"}')
    verdict = await judge_workflow_step(mock, wf, wf.steps[0], "hi", "Hello! Welcome.")
    assert verdict == "yes"


@pytest.mark.asyncio
async def test_judge_no_and_maybe() -> None:
    wf = _wf()
    for scripted, expected in [('{"verdict": "no"}', "no"), ('{"verdict": "maybe"}', "maybe")]:
        mock = MockLlmProvider()
        mock.push_text(scripted)
        assert await judge_workflow_step(mock, wf, wf.steps[0], "u", "reply") == expected


@pytest.mark.asyncio
async def test_judge_tolerates_prose_around_json() -> None:
    wf = _wf()
    mock = MockLlmProvider()
    mock.push_text('Sure — here is my call: {"verdict": "yes", "reason": "ok"} done.')
    assert await judge_workflow_step(mock, wf, wf.steps[0], "u", "r") == "yes"


@pytest.mark.asyncio
async def test_judge_unparseable_defaults_to_no() -> None:
    wf = _wf()
    mock = MockLlmProvider()
    mock.push_text("I really cannot tell honestly")
    # No parseable verdict and no "yes"/"maybe" keyword → stay put ("no").
    assert await judge_workflow_step(mock, wf, wf.steps[0], "u", "r") == "no"


@pytest.mark.asyncio
async def test_judge_skips_when_nothing_to_evaluate() -> None:
    wf = _wf()
    assert await judge_workflow_step(MockLlmProvider(), None, None, "u", "r") == "skipped"
    assert await judge_workflow_step(MockLlmProvider(), wf, wf.steps[0], "u", "   ") == "skipped"


@pytest.mark.asyncio
async def test_judge_failure_tolerant() -> None:
    wf = _wf()
    # No chat client → cannot judge, but must not raise; stays on current step.
    assert await judge_workflow_step(None, wf, wf.steps[0], "u", "r") == "no"

    mock = MockLlmProvider()
    mock.push_error(RuntimeError("gateway down"))
    assert await judge_workflow_step(mock, wf, wf.steps[0], "u", "r") == "no"
