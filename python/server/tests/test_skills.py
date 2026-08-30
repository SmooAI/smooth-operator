"""Skill resolution parity (th-ebe27d / Rust PR #338): the wire carries *intent*
(``send_message.skill``), the server resolves it and composes the body into the turn's
system prompt, and an unknown skill fails CLOSED with ``SKILL_NOT_FOUND`` rather than
silently degrading into an unskilled answer.

The first five tests mirror ``rust/smooth-operator-server/src/skills.rs`` case for case,
under the same names; the rest cover the dispatcher wiring the Rust side tests in
``tests/skill_field.rs``.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from smooth_operator_core import MockLlmProvider

from smooth_operator_server.agent_config import StaticAgentConfigResolver
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.skills import (
    SKILLS_DIR_ENV,
    DirSkillResolver,
    is_valid_skill_name,
    resolve_section,
    skill_section,
    strip_frontmatter,
)


def _write_skill(root: Path, name: str, body: str, *, frontmatter: bool = True) -> None:
    d = root / name
    d.mkdir(parents=True, exist_ok=True)
    text = f"---\nname: {name}\n---\n{body}\n" if frontmatter else body
    (d / "SKILL.md").write_text(text, encoding="utf-8")


# ── parity with the Rust unit tests ──────────────────────────────────────────


def test_rejects_traversal_and_separators() -> None:
    assert is_valid_skill_name("code-review")
    assert is_valid_skill_name("add_show")
    assert not is_valid_skill_name("")
    assert not is_valid_skill_name("..")
    assert not is_valid_skill_name("../../etc/passwd")
    assert not is_valid_skill_name("a/b")
    assert not is_valid_skill_name("a\\b")
    assert not is_valid_skill_name("a b")
    assert not is_valid_skill_name("a" * 129)
    assert is_valid_skill_name("a" * 128)
    # A NUL can't sneak through as a truncating path terminator.
    assert not is_valid_skill_name("ok\x00.md")


def test_strips_frontmatter_only_when_well_formed() -> None:
    assert strip_frontmatter("---\nname: x\ndescription: y\n---\nBody here\n") == "Body here\n"
    # No frontmatter → untouched.
    assert strip_frontmatter("Body here\n") == "Body here\n"
    # Unterminated → untouched (don't swallow the file).
    assert strip_frontmatter("---\nname: x\n") == "---\nname: x\n"
    # A `---` mid-body (a markdown rule) after real frontmatter still closes at the
    # FIRST fence, which is the frontmatter's.
    assert strip_frontmatter("---\nname: x\n---\nintro\n\n---\n\nmore\n") == "intro\n\n---\n\nmore\n"


@pytest.mark.asyncio
async def test_dir_resolver_reads_first_matching_root(tmp_path: Path) -> None:
    high, low = tmp_path / "high", tmp_path / "low"
    _write_skill(high, "greet", "HIGH BODY")
    _write_skill(low, "greet", "LOW BODY")

    resolver = DirSkillResolver([high, low])
    assert await resolver.resolve("greet") == "HIGH BODY"
    assert await resolver.resolve("nope") is None
    # Traversal can't escape the root even though the file exists one level up.
    assert await resolver.resolve("../low/greet") is None

    # A missing first root falls through to the next one's copy.
    resolver = DirSkillResolver([tmp_path / "missing", low])
    assert await resolver.resolve("greet") == "LOW BODY"


def test_path_list_parsing_is_off_when_empty() -> None:
    assert DirSkillResolver.from_path_list("") is None
    assert DirSkillResolver.from_path_list("  : ") is None
    parsed = DirSkillResolver.from_path_list("/a: /b :")
    assert parsed is not None
    assert parsed.roots == [Path("/a"), Path("/b")]


@pytest.mark.asyncio
async def test_resolve_section_composes_and_reports_unknown(tmp_path: Path) -> None:
    _write_skill(tmp_path, "review", "Check the diff.", frontmatter=False)
    resolver = DirSkillResolver([tmp_path])

    section = await resolve_section(resolver, "review")
    assert section is not None
    assert section.startswith("## Skill: review\n")
    assert section.endswith("Check the diff.")

    assert await resolve_section(resolver, "unknown") is None
    # No resolver installed ⇒ every skill is unknown.
    assert await resolve_section(None, "review") is None


@pytest.mark.asyncio
async def test_from_env_reads_the_documented_var(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _write_skill(tmp_path, "greet", "ENV BODY")
    monkeypatch.setenv(SKILLS_DIR_ENV, str(tmp_path))
    resolver = DirSkillResolver.from_env()
    assert resolver is not None
    assert await resolver.resolve("greet") == "ENV BODY"

    monkeypatch.delenv(SKILLS_DIR_ENV)
    assert DirSkillResolver.from_env() is None


@pytest.mark.asyncio
async def test_empty_body_is_not_a_skill(tmp_path: Path) -> None:
    """A SKILL.md that is nothing but frontmatter has no instructions to follow, so it
    is unknown rather than an empty section appended to the prompt."""
    _write_skill(tmp_path, "hollow", "")
    assert await DirSkillResolver([tmp_path]).resolve("hollow") is None


# ── dispatcher wiring ────────────────────────────────────────────────────────


async def _session_and_dispatcher(**kwargs):
    store = InMemorySessionStore()
    session = await store.create_session("agent-x", None, None)
    dispatcher = FrameDispatcher(
        store, MockLlmProvider(), tools=[], agent_config_resolver=StaticAgentConfigResolver({}), **kwargs
    )
    return session, dispatcher


def _system_prompt(chat_client) -> str:
    """The system message the turn actually sent — the thing the skill must land in.
    Asserting on a json dump of ALL messages would also match the user message, which is
    precisely what this seam is supposed to keep the skill OUT of."""
    system = chat_client.calls[0].messages[0]
    assert system["role"] == "system", "turn must open with a system message"
    return system["content"]


def _send(session_id: str, message: str, skill: str | None = None) -> str:
    frame: dict = {"action": "send_message", "sessionId": session_id, "message": message}
    if skill is not None:
        frame["skill"] = skill
    return json.dumps(frame)


@pytest.mark.asyncio
async def test_unknown_skill_fails_closed_before_the_ack(tmp_path: Path) -> None:
    """The turn either runs WITH the skill or does not run at all: the error must land
    instead of the 202, not after it, or the client holds an accepted turn that errors."""
    session, dispatcher = await _session_and_dispatcher(skill_resolver=DirSkillResolver([tmp_path]))

    events: list[dict] = []
    await dispatcher.dispatch(_send(session.session_id, "review this", skill="nope"), events.append)
    await dispatcher.wait_for_turns()

    assert [e["type"] for e in events] == ["error"]
    assert events[0]["error"]["code"] == "SKILL_NOT_FOUND"
    assert events[0]["data"]["error"]["code"] == "SKILL_NOT_FOUND"


@pytest.mark.asyncio
async def test_skill_with_no_resolver_installed_is_not_found() -> None:
    """Default deployment: no resolver ⇒ a skill field is a clean SKILL_NOT_FOUND, so a
    multi-tenant server never serves host skills by accident."""
    session, dispatcher = await _session_and_dispatcher()

    events: list[dict] = []
    await dispatcher.dispatch(_send(session.session_id, "review this", skill="review"), events.append)
    await dispatcher.wait_for_turns()

    assert [e["type"] for e in events] == ["error"]
    assert events[0]["error"]["code"] == "SKILL_NOT_FOUND"


@pytest.mark.asyncio
async def test_known_skill_reaches_the_system_prompt_not_the_user_message(tmp_path: Path) -> None:
    """The whole point of the seam: the body lands in the system prompt and the
    persisted/sent user message stays exactly what the user typed."""
    _write_skill(tmp_path, "review", "Be adversarial about the diff.")
    session, dispatcher = await _session_and_dispatcher(skill_resolver=DirSkillResolver([tmp_path]))
    dispatcher._chat_client.push_text("done")  # noqa: SLF001 — the mock IS the seam under test

    events: list[dict] = []
    await dispatcher.dispatch(_send(session.session_id, "look at PR 12", skill="review"), events.append)
    await dispatcher.wait_for_turns()

    assert [e["type"] for e in events][0] == "immediate_response"
    system = _system_prompt(dispatcher._chat_client)  # noqa: SLF001
    assert "## Skill: review" in system
    assert "Be adversarial about the diff." in system
    # The user message is untouched — the skill body never enters conversation history,
    # so it is not replayed as context on every later turn (the reason for the seam).
    user = dispatcher._chat_client.calls[0].messages[-1]  # noqa: SLF001
    assert user["content"] == "look at PR 12"
    assert "adversarial" not in json.dumps(user)


@pytest.mark.asyncio
async def test_no_skill_field_leaves_the_prompt_unchanged(tmp_path: Path) -> None:
    """Back-compat: a turn without `skill` behaves byte-identically to before."""
    _write_skill(tmp_path, "review", "Be adversarial.")
    session, dispatcher = await _session_and_dispatcher(skill_resolver=DirSkillResolver([tmp_path]))
    dispatcher._chat_client.push_text("done")  # noqa: SLF001

    events: list[dict] = []
    await dispatcher.dispatch(_send(session.session_id, "hello"), events.append)
    await dispatcher.wait_for_turns()

    assert "## Skill:" not in _system_prompt(dispatcher._chat_client)  # noqa: SLF001


@pytest.mark.asyncio
async def test_blank_skill_is_ignored_not_rejected(tmp_path: Path) -> None:
    """An empty/whitespace `skill` is "no skill", not an unknown one — a client that
    always sends the field must not be unable to run an ordinary turn."""
    session, dispatcher = await _session_and_dispatcher(skill_resolver=DirSkillResolver([tmp_path]))
    dispatcher._chat_client.push_text("done")  # noqa: SLF001

    events: list[dict] = []
    await dispatcher.dispatch(_send(session.session_id, "hello", skill="   "), events.append)
    await dispatcher.wait_for_turns()

    assert [e["type"] for e in events][0] == "immediate_response"
    assert not any(e["type"] == "error" for e in events)


def test_skill_section_shape_is_identical_across_languages() -> None:
    """The framing line is wire-visible behavior — it is what tells the model the skill
    applies to this turn — so it is pinned, not incidental."""
    assert skill_section("code-review", "BODY") == (
        "## Skill: code-review\n\nThe user invoked this skill for this turn. Follow it.\n\nBODY"
    )
