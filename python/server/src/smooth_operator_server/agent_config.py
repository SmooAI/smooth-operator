"""Per-agent configuration — instructions + structured conversation workflow.

The monorepo's ``agents`` table gives each agent its own ``instructions`` (jsonb
``{prompt: string}``) and ``conversation_workflow`` (jsonb). This module models
those shapes for the server and parses them **tolerantly**: malformed config
degrades to ``None`` (the caller falls back to the org/server default) and never
crashes a session. Authoritative shapes:
``packages/schemas/src/agents/agent.ts`` (``ConversationWorkflow`` /
``ConversationWorkflowStep``).

The server resolves an :class:`AgentConfig` per turn (keyed by the session's
``agent_id``) and threads it into the runner's prompt assembly + workflow judge.
With no config registered for an agent, resolution returns ``None`` and behavior
is byte-for-byte unchanged (the runner stays on its server-wide default prompt).
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import Any

#: Schema bounds mirrored from ``agent.ts`` — steps beyond these are clamped/dropped
#: rather than rejected wholesale, so one malformed step never voids a workable
#: workflow (tolerant parse). ``MAX_STEPS`` matches the Zod ``.max(20)``.
MAX_STEPS = 20


@dataclass(frozen=True)
class ConversationWorkflowStep:
    """One step in a structured conversation workflow (mirrors ``ConversationWorkflowStep``)."""

    id: str
    intent: str
    criteria: str
    next: str | None = None


@dataclass(frozen=True)
class ConversationWorkflow:
    """A goal + ordered steps the agent works through (mirrors ``ConversationWorkflow``)."""

    goal: str
    steps: list[ConversationWorkflowStep]


@dataclass(frozen=True)
class AgentConfig:
    """Resolved per-agent behavior. Every field is optional — an agent may set only
    its ``instructions`` prompt, only a workflow, or both."""

    #: The agent's freeform system-prompt body (``instructions.prompt``). ``None`` →
    #: the runner falls back to the server-wide default prompt.
    instructions: str | None = None
    #: Free-text personality note appended to the persona preamble when present.
    personality: str | None = None
    #: First-turn greeting seed (surfaced to the runner's greeting awareness).
    greeting: str | None = None
    #: Structured conversation workflow. ``None`` → freeform prompt only.
    conversation_workflow: ConversationWorkflow | None = None
    #: Tool allow-list: when non-empty, this agent's turns are restricted to the
    #: server tools whose names appear here (empty → the full server tool set).
    allowed_tools: list[str] = field(default_factory=list)

    @property
    def is_empty(self) -> bool:
        """True when nothing is configured — resolution can treat it as ``None``."""
        return (
            self.instructions is None
            and self.personality is None
            and self.greeting is None
            and self.conversation_workflow is None
            and not self.allowed_tools
        )


def _clean_str(value: Any) -> str | None:
    """A non-empty trimmed string, or ``None`` (tolerant: any non-str → ``None``)."""
    if not isinstance(value, str):
        return None
    trimmed = value.strip()
    return trimmed or None


def parse_workflow(raw: Any) -> ConversationWorkflow | None:
    """Parse a ``conversation_workflow`` jsonb value tolerantly.

    Returns ``None`` (degrade to freeform) when the value is missing, the wrong
    shape, has no valid steps, or lacks a goal — never raises. Individual steps
    missing a required field (``id``/``intent``/``criteria``) are dropped; the
    remainder still forms a usable workflow. Steps beyond :data:`MAX_STEPS` are
    trimmed (matches the schema's ``.max(20)``)."""
    if not isinstance(raw, dict):
        return None
    goal = _clean_str(raw.get("goal"))
    raw_steps = raw.get("steps")
    if goal is None or not isinstance(raw_steps, list):
        return None

    steps: list[ConversationWorkflowStep] = []
    for item in raw_steps:
        if len(steps) >= MAX_STEPS:
            break
        if not isinstance(item, dict):
            continue
        step_id = _clean_str(item.get("id"))
        intent = _clean_str(item.get("intent"))
        criteria = _clean_str(item.get("criteria"))
        if step_id is None or intent is None or criteria is None:
            continue
        steps.append(
            ConversationWorkflowStep(
                id=step_id,
                intent=intent,
                criteria=criteria,
                next=_clean_str(item.get("next")),
            )
        )

    if not steps:
        return None
    return ConversationWorkflow(goal=goal, steps=steps)


def parse_agent_config(raw: Any) -> AgentConfig | None:
    """Parse a per-agent config dict (the ``agents`` row projection) tolerantly.

    ``instructions`` is the jsonb ``{prompt: string}`` shape (a bare string is also
    accepted). Any malformed sub-field degrades to its default rather than raising,
    so a partially-bad config still yields a usable :class:`AgentConfig`. Returns
    ``None`` when the input isn't a dict or resolves to nothing configured."""
    if not isinstance(raw, dict):
        return None

    instructions_raw = raw.get("instructions")
    if isinstance(instructions_raw, dict):
        instructions = _clean_str(instructions_raw.get("prompt"))
    else:
        instructions = _clean_str(instructions_raw)

    # tool_config (snake) / allowedTools (camel) — a string-array tool allow-list.
    tools_raw = raw.get("tool_config")
    if not tools_raw:
        tools_raw = raw.get("allowedTools")
    allowed_tools = (
        [name for name in tools_raw if isinstance(name, str) and name.strip()] if isinstance(tools_raw, list) else []
    )

    config = AgentConfig(
        instructions=instructions,
        personality=_clean_str(raw.get("personality")),
        greeting=_clean_str(raw.get("greeting")),
        conversation_workflow=parse_workflow(raw.get("conversation_workflow")),
        allowed_tools=allowed_tools,
    )
    return None if config.is_empty else config


def filter_tools(tools: list[Any], config: AgentConfig | None) -> list[Any]:
    """Restrict ``tools`` to the agent's allow-list (matched by ``.name``). An empty
    allow-list or ``None`` config returns ``tools`` unchanged, so an un-configured
    agent keeps the full server tool set; unknown names in the allow-list are
    ignored. Mirrors the Go/TS ``filterTools``."""
    if config is None or not config.allowed_tools:
        return tools
    allowed = set(config.allowed_tools)
    return [t for t in tools if t.name in allowed]


class AgentConfigResolver(ABC):
    """Resolves an ``agentId`` to its :class:`AgentConfig`, server-side.

    The config-delivery seam: the ws protocol's ``create_conversation_session``
    carries only an agent UUID, so per-agent config is looked up here (mirrors the
    Rust resolver + the TS ``AgentConfigResolver``, and sits alongside the auth
    verifier seam). A multi-tenant host implements this against the `agents` table;
    the reference server uses :class:`StaticAgentConfigResolver`."""

    @abstractmethod
    async def resolve(self, agent_id: str) -> AgentConfig | None:
        """The agent's config, or ``None`` → the server-wide default drives the turn."""
        ...


class StaticAgentConfigResolver(AgentConfigResolver):
    """Dict-backed resolver keyed by ``agentId``. The default (empty mapping) is the
    no-op resolver — every lookup returns ``None`` so behavior is unchanged."""

    def __init__(self, configs: dict[str, AgentConfig] | None = None) -> None:
        self._configs = configs or {}

    async def resolve(self, agent_id: str) -> AgentConfig | None:
        return self._configs.get(agent_id)
