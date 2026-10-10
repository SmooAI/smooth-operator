"""Prompt composition seam (SMOODEV-3798) — parity with the Rust reference's
``smooth_operator_server::prompt_composer``.

A :class:`PromptComposer` assembles the turn's system prompt from its ordered
sections plus who is asking and which agent answers. The runner drops blank
sections and joins the rest with a blank line (:func:`join_sections`). With no
composer installed the runner uses :class:`DefaultPromptComposer`, which keeps
this server's historical order: base (with the agent's personality), first-turn
greeting, workflow step, then the invoked skill.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import Any, Protocol


class BaseSource(str, Enum):
    """Where :attr:`PromptSections.base` came from."""

    AGENT = "agent"
    """The answering agent's own instructions."""
    ORG_PERSONA = "org_persona"
    """The org's saved persona override."""
    DEFAULT_PERSONA = "default_persona"
    """The host's server-wide system prompt."""
    BUILT_IN = "built_in"
    """The server's built-in prompt."""


@dataclass(frozen=True)
class PromptSections:
    """Every input to one turn's system prompt. ``None`` ⇒ the section does not
    apply to this turn."""

    base: str
    base_source: BaseSource
    greeting: str | None = None
    workflow: str | None = None
    skill: str | None = None
    suggested_replies: str = ""
    agent: Any = None
    access: Any = None
    session_authenticated: bool = False
    conversation_id: str = ""


class PromptComposer(Protocol):
    """Assembles the turn's system prompt. Return the sections in order; blank
    strings are dropped and the rest joined with ``"\\n\\n"``."""

    def compose(self, sections: PromptSections) -> list[str]: ...


class DefaultPromptComposer:
    """The historical order: base, greeting, workflow, skill, suggested replies."""

    @staticmethod
    def sections(sections: PromptSections) -> list[str]:
        """The default ordered sections, for hosts that extend rather than reorder."""
        out = [sections.base]
        for part in (sections.greeting, sections.workflow, sections.skill):
            if part is not None:
                out.append(part)
        out.append(sections.suggested_replies)
        return out

    def compose(self, sections: PromptSections) -> list[str]:
        return self.sections(sections)


def join_sections(sections: list[str]) -> str:
    """Drop blank sections and join the rest with a blank line."""
    return "\n\n".join(s for s in sections if s and s.strip())


def render_prompt(composer: PromptComposer | None, sections: PromptSections) -> str:
    """Compose ``sections`` with ``composer`` (or the default) and join."""
    return join_sections((composer or DefaultPromptComposer()).compose(sections))
