"""SMOODEV-3342 — default model parity with the Rust reference.

Mirrors ``smooth-operator-server`` ``config.rs``
(``defaults_apply_when_env_absent`` / ``judge_model_defaults_to_groq_and_env_overrides``):
the main turn defaults to ``gpt-6-luna`` and the workflow judge has its OWN default,
``groq-gpt-oss-120b`` — and a turn with no pinned model REQUESTS the server default
rather than letting the engine substitute its built-in fallback model.
"""

from __future__ import annotations

import pytest
from smooth_operator_core import MockLlmProvider

from smooth_operator_server.admin import _default_settings
from smooth_operator_server.agent_config import (
    AgentConfig,
    ConversationWorkflow,
    ConversationWorkflowStep,
    StaticAgentConfigResolver,
)
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import DEFAULT_MODEL
from smooth_operator_server.workflow import JUDGE_MAX_TOKENS, WORKFLOW_JUDGE_MODEL


def test_default_model_is_gpt_6_luna() -> None:
    assert DEFAULT_MODEL == "gpt-6-luna"


def test_judge_default_is_groq_and_distinct_from_turn_model() -> None:
    assert WORKFLOW_JUDGE_MODEL == "groq-gpt-oss-120b"
    assert WORKFLOW_JUDGE_MODEL != DEFAULT_MODEL


def test_unsaved_org_settings_report_default_model() -> None:
    assert _default_settings("org-1")["model"] == DEFAULT_MODEL


@pytest.mark.asyncio
async def test_turn_and_judge_request_their_defaults_when_unpinned() -> None:
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

    # No model / judge_model pinned — the server defaults must go on the wire.
    dispatcher = FrameDispatcher(
        store,
        mock,
        agent_config_resolver=StaticAgentConfigResolver({"agent-x": config}),
    )
    await dispatcher.dispatch(
        '{"action":"send_message","sessionId":"%s","message":"hi"}' % session.session_id,
        lambda _e: None,
    )
    await dispatcher.wait_for_turns()

    models = [c.kwargs.get("model") for c in mock.calls]
    assert models == [DEFAULT_MODEL, WORKFLOW_JUDGE_MODEL]
    # The default judge is a reasoning model: the cap must leave room for reasoning
    # (parity with the Rust JUDGE_MAX_TOKENS = 512).
    assert JUDGE_MAX_TOKENS == 512
    assert mock.calls[1].kwargs.get("max_tokens") == JUDGE_MAX_TOKENS
    assert await store.get_current_step_id(session.conversation_id) == "s2"
