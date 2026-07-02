"""Unit tests for tolerant per-agent config parsing (SMOODEV-590).

Malformed config must degrade to ``None`` (freeform default) and never raise —
a bad `agents` row can't crash a session.
"""

from __future__ import annotations

import pytest

from smooth_operator_server.agent_config import (
    MAX_STEPS,
    AgentConfig,
    StaticAgentConfigResolver,
    parse_agent_config,
    parse_workflow,
)


def test_parse_workflow_full() -> None:
    workflow = parse_workflow(
        {
            "goal": "Qualify the lead",
            "steps": [
                {"id": "greet", "intent": "Greet", "criteria": "greeted", "next": "qualify"},
                {"id": "qualify", "intent": "Qualify", "criteria": "budget known"},
            ],
        }
    )
    assert workflow is not None
    assert workflow.goal == "Qualify the lead"
    assert [s.id for s in workflow.steps] == ["greet", "qualify"]
    assert workflow.steps[0].next == "qualify"
    assert workflow.steps[1].next is None


def test_parse_workflow_drops_incomplete_steps_keeps_rest() -> None:
    workflow = parse_workflow(
        {
            "goal": "Goal",
            "steps": [
                {"id": "ok", "intent": "i", "criteria": "c"},
                {"id": "missing-criteria", "intent": "i"},  # dropped
                {"intent": "no id", "criteria": "c"},  # dropped
                "not-a-dict",  # dropped
                {"id": "ok2", "intent": "i", "criteria": "c"},
            ],
        }
    )
    assert workflow is not None
    assert [s.id for s in workflow.steps] == ["ok", "ok2"]


def test_parse_workflow_trims_to_max_steps() -> None:
    steps = [{"id": f"s{i}", "intent": "i", "criteria": "c"} for i in range(MAX_STEPS + 5)]
    workflow = parse_workflow({"goal": "g", "steps": steps})
    assert workflow is not None
    assert len(workflow.steps) == MAX_STEPS


def test_parse_workflow_malformed_returns_none() -> None:
    # Not a dict, missing goal, missing/empty steps, no valid steps → all None.
    assert parse_workflow(None) is None
    assert parse_workflow("nope") is None
    assert parse_workflow({"steps": [{"id": "a", "intent": "i", "criteria": "c"}]}) is None
    assert parse_workflow({"goal": "g"}) is None
    assert parse_workflow({"goal": "g", "steps": []}) is None
    assert parse_workflow({"goal": "g", "steps": [{"id": "a"}]}) is None
    assert parse_workflow({"goal": "   ", "steps": [{"id": "a", "intent": "i", "criteria": "c"}]}) is None


def test_parse_agent_config_instructions_object_shape() -> None:
    config = parse_agent_config({"instructions": {"prompt": "Be terse."}})
    assert config is not None
    assert config.instructions == "Be terse."


def test_parse_agent_config_instructions_bare_string() -> None:
    config = parse_agent_config({"instructions": "Be terse."})
    assert config is not None
    assert config.instructions == "Be terse."


def test_parse_agent_config_all_fields() -> None:
    config = parse_agent_config(
        {
            "instructions": {"prompt": "Help the user."},
            "personality": "warm",
            "greeting": "Hi there!",
            "tool_config": {"crm": {"enabled": True}},
            "conversation_workflow": {"goal": "g", "steps": [{"id": "a", "intent": "i", "criteria": "c"}]},
        }
    )
    assert config is not None
    assert config.instructions == "Help the user."
    assert config.personality == "warm"
    assert config.greeting == "Hi there!"
    assert config.tool_config == {"crm": {"enabled": True}}
    assert config.conversation_workflow is not None


def test_parse_agent_config_bad_subfields_degrade_not_raise() -> None:
    # Bad workflow + bad tool_config degrade to defaults; the good instructions survive.
    config = parse_agent_config(
        {
            "instructions": {"prompt": "Keep going."},
            "personality": 123,  # not a str → None
            "conversation_workflow": "garbage",  # → None
            "tool_config": ["not", "a", "dict"],  # → {}
        }
    )
    assert config is not None
    assert config.instructions == "Keep going."
    assert config.personality is None
    assert config.conversation_workflow is None
    assert config.tool_config == {}


def test_parse_agent_config_empty_returns_none() -> None:
    assert parse_agent_config(None) is None
    assert parse_agent_config("nope") is None
    assert parse_agent_config({}) is None
    assert parse_agent_config({"instructions": {"prompt": "   "}}) is None
    assert parse_agent_config({"instructions": None, "personality": None}) is None


def test_agent_config_is_empty() -> None:
    assert AgentConfig().is_empty
    assert not AgentConfig(instructions="x").is_empty
    assert not AgentConfig(tool_config={"a": 1}).is_empty


@pytest.mark.asyncio
async def test_static_resolver() -> None:
    config = AgentConfig(instructions="hi")
    resolver = StaticAgentConfigResolver({"a": config})
    assert await resolver.resolve("a") is config
    assert await resolver.resolve("missing") is None
    # Empty resolver is the no-op default.
    assert await StaticAgentConfigResolver().resolve("a") is None
