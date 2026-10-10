"""PromptComposer seam (SMOODEV-3798) — parity with the Rust reference's
``prompt_composer`` tests: the default order, blank sections dropped, a host
composer that appends last and sees the context, a reorder, and base_source."""

from __future__ import annotations

import pytest
from smooth_operator_core import MockLlmProvider

from smooth_operator_server.agent_config import AgentConfig
from smooth_operator_server.prompt_composer import (
    BaseSource,
    DefaultPromptComposer,
    PromptSections,
    join_sections,
    render_prompt,
)
from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import DEFAULT_SYSTEM_PROMPT, TurnRunner


def _sections(**kw) -> PromptSections:
    base = dict(
        base="BASE",
        base_source=BaseSource.AGENT,
        greeting="GREETING",
        workflow="WORKFLOW",
        skill="SKILL",
        suggested_replies="TRAILER",
        conversation_id="c1",
    )
    base.update(kw)
    return PromptSections(**base)


def test_default_composer_keeps_the_historical_order() -> None:
    assert render_prompt(None, _sections()) == "BASE\n\nGREETING\n\nWORKFLOW\n\nSKILL\n\nTRAILER"


def test_absent_sections_are_skipped() -> None:
    assert render_prompt(None, _sections(greeting=None, workflow=None, skill=None)) == "BASE\n\nTRAILER"


def test_join_drops_blank_sections() -> None:
    assert join_sections(["a", "", "  ", "b"]) == "a\n\nb"


def test_host_composer_appends_last_and_sees_the_context() -> None:
    class Safety:
        def compose(self, s: PromptSections) -> list[str]:
            return [
                *DefaultPromptComposer.sections(s),
                f"SAFETY(source={s.base_source.value}, authed={s.session_authenticated}, conv={s.conversation_id})",
            ]

    out = render_prompt(Safety(), _sections())
    assert out.startswith("BASE\n\nGREETING")
    assert out.endswith("TRAILER\n\nSAFETY(source=agent, authed=False, conv=c1)")


def test_host_composer_may_reorder() -> None:
    class Reorder:
        def compose(self, s: PromptSections) -> list[str]:
            return [s.base, s.workflow or "", s.suggested_replies]

    assert render_prompt(Reorder(), _sections()) == "BASE\n\nWORKFLOW\n\nTRAILER"


class _Capture:
    def __init__(self) -> None:
        self.seen: PromptSections | None = None

    def compose(self, s: PromptSections) -> list[str]:
        self.seen = s
        return [*DefaultPromptComposer.sections(s), "LAST"]


async def _turn(composer, *, system_prompt=None, agent_config=None, authed=False) -> tuple[str, MockLlmProvider]:
    mock = MockLlmProvider()
    mock.push_text("hi")
    runner = TurnRunner(
        chat_client=mock,
        store=InMemorySessionStore(),
        system_prompt=system_prompt,
        agent_config=agent_config,
        prompt_composer=composer,
        session_authenticated=authed,
    )
    await runner.run(conversation_id="conv-9", request_id="r-1", user_message="hello", sink=lambda _e: None)
    prompt = next(m["content"] for m in mock.calls[0].messages if m["role"] == "system")
    return prompt, mock


@pytest.mark.asyncio
async def test_runner_composes_through_the_host_composer() -> None:
    cap = _Capture()
    prompt, _ = await _turn(cap, agent_config=AgentConfig(instructions="Be Ada.", greeting="Hi!"), authed=True)
    assert prompt.startswith("Be Ada.")
    assert prompt.endswith("LAST")
    assert cap.seen is not None
    assert cap.seen.base_source is BaseSource.AGENT
    assert cap.seen.session_authenticated is True
    assert cap.seen.conversation_id == "conv-9"
    assert cap.seen.greeting is not None  # first turn


@pytest.mark.asyncio
async def test_base_source_tracks_the_fallback_prompt() -> None:
    cap = _Capture()
    await _turn(cap)
    assert cap.seen is not None and cap.seen.base_source is BaseSource.BUILT_IN
    cap = _Capture()
    await _turn(cap, system_prompt="Host persona.")
    assert cap.seen is not None and cap.seen.base_source is BaseSource.DEFAULT_PERSONA


@pytest.mark.asyncio
async def test_no_composer_is_unchanged() -> None:
    prompt, _ = await _turn(None)
    assert prompt == DEFAULT_SYSTEM_PROMPT
