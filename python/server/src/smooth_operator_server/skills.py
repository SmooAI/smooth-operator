"""Skill resolution — the engine side of ``send_message.skill`` (Rust PR #338).

A *skill* is a named, reusable recipe (a markdown body). Before this seam every
client resolved the skill itself and prepended the body to the message text, so
the wire carried prose — and the body persisted into conversation history, where
it was replayed as context on every later turn. Now the wire carries **intent**
(``skill: "code-review"``) and the server composes it into the turn's system
prompt, leaving the persisted user message exactly what the user typed.

Two pieces, mirroring ``rust/smooth-operator-server/src/skills.rs``:

- :class:`SkillResolver` — the host seam, installed via ``serve(skill_resolver=...)``.
- :class:`DirSkillResolver` — the working default: ``<root>/<name>/SKILL.md`` over
  the roots in ``SMOOTH_SKILLS_DIR`` (a ``:``-separated list, first match wins).
  Unset ⇒ no resolver is installed and any ``skill`` field is a clean
  ``SKILL_NOT_FOUND``, so a multi-tenant deploy never reads host skills by accident.
"""

from __future__ import annotations

import os
import re
from pathlib import Path
from typing import Protocol, runtime_checkable

#: Env var naming the skill roots for :class:`DirSkillResolver`: ``:``-separated,
#: searched in order.
SKILLS_DIR_ENV = "SMOOTH_SKILLS_DIR"

#: The one legal shape of a skill name — see :func:`is_valid_skill_name`.
_NAME_RE = re.compile(r"^[A-Za-z0-9_-]+$")


@runtime_checkable
class SkillResolver(Protocol):
    """Resolves a skill name to its markdown body.

    ``None`` means "unknown skill" — the dispatcher turns that into a
    ``SKILL_NOT_FOUND`` error and does NOT run the turn, so a typo'd skill never
    silently degrades into an unskilled answer."""

    async def resolve(self, name: str) -> str | None:
        """The skill's markdown body, or ``None`` when no such skill exists."""
        ...


def skill_section(name: str, body: str) -> str:
    """Render a resolved skill as a system-prompt section.

    The skill moved from the *user message* (where clients used to prepend it) to
    the *system prompt*, so this framing line is what tells the model the skill
    applies to this turn."""
    return f"## Skill: {name}\n\nThe user invoked this skill for this turn. Follow it.\n\n{body}"


def is_valid_skill_name(name: str) -> bool:
    """Whether ``name`` is a legal skill name.

    Deliberately strict: ASCII alphanumerics, ``-`` and ``_`` only. That is the
    kebab-case convention skills already use, and it makes path traversal
    (``..``, ``/``, ``\\``, NUL) *unrepresentable* rather than filtered — the name
    is joined onto a filesystem root by :class:`DirSkillResolver`."""
    return 0 < len(name) <= 128 and _NAME_RE.match(name) is not None


def strip_frontmatter(text: str) -> str:
    """Strip a leading YAML frontmatter block (``---`` … ``---``), returning the body.

    SKILL.md files carry frontmatter (description, triggers, allowed tools) that is
    discovery metadata, not instructions — the model should see only the body.
    Unterminated frontmatter is returned untouched rather than swallowing the file."""
    if not text.startswith("---\n"):
        return text
    rest = text[4:]
    # The closing fence is a line that is exactly `---`.
    idx = rest.find("---")
    while idx != -1:
        at_line_start = idx == 0 or rest[idx - 1] == "\n"
        rest_of_line = rest[idx + 3 :]
        if at_line_start and rest_of_line.startswith("\n"):
            return rest_of_line.lstrip("\n")
        idx = rest.find("---", idx + 1)
    return text


class DirSkillResolver:
    """The default resolver: reads ``<root>/<name>/SKILL.md``, first root wins."""

    def __init__(self, roots: list[Path]) -> None:
        self.roots = roots

    @classmethod
    def from_env(cls) -> DirSkillResolver | None:
        """Build from :data:`SKILLS_DIR_ENV`.

        ``None`` when the var is unset or names no non-empty root, so the caller
        installs nothing and the feature stays off by default."""
        raw = os.environ.get(SKILLS_DIR_ENV)
        return None if raw is None else cls.from_path_list(raw)

    # ponytail: ':' hardcoded to match the Rust reference rather than os.pathsep. On
    # Windows that makes a drive-qualified root ("C:\\skills") unrepresentable; change
    # it in every lane at once or they diverge.
    @classmethod
    def from_path_list(cls, raw: str) -> DirSkillResolver | None:
        """Build from a ``:``-separated path list — the parsed half of
        :meth:`from_env`, so it is testable without touching the process environment."""
        roots = [Path(part) for part in (p.strip() for p in raw.split(":")) if part]
        return cls(roots) if roots else None

    async def resolve(self, name: str) -> str | None:
        if not is_valid_skill_name(name):
            return None
        for root in self.roots:
            try:
                # ponytail: blocking read on the event loop. A SKILL.md is a few KB off
                # local disk; move to a thread if a resolver ever fronts network storage.
                text = (root / name / "SKILL.md").read_text(encoding="utf-8")
            except OSError:
                continue
            body = strip_frontmatter(text).strip()
            if body:
                return body
        return None


async def resolve_section(resolver: SkillResolver | None, name: str) -> str | None:
    """Resolve ``name`` through ``resolver`` and render it as a system-prompt section.

    ``None`` when there is no resolver installed or the skill is unknown — both are
    ``SKILL_NOT_FOUND`` to the client (the distinction is a deployment detail the
    caller should not have to guess at)."""
    if resolver is None:
        return None
    body = await resolver.resolve(name)
    return None if body is None else skill_section(name, body)
