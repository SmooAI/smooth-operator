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
class EnabledTool:
    """One entry in ``tool_config.enabledTools`` (mirrors the monorepo ``AgentToolConfig``).
    ``auth_level``/``config`` are preserved on the parsed type for downstream hosts even
    though the reference server doesn't act on them yet."""

    tool_id: str
    enabled: bool = True
    auth_level: str = "none"
    config: dict[str, Any] = field(default_factory=dict)


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
    #: ``tool_config.enabledTools`` — a tool allow-list. When non-empty, this agent's
    #: turns are restricted to the ``enabled=true`` entries' ``tool_id`` (empty → the
    #: full server tool set).
    enabled_tools: list[EnabledTool] = field(default_factory=list)
    #: Agent visibility — ``"public"`` (embeddable widget, no auth session) or
    #: ``"internal"`` (authenticated dashboard). Gates admin-level tools and decides
    #: whether ``end_user`` auth is auto-satisfied. Defaults to ``"public"``.
    visibility: str = "public"

    @property
    def is_empty(self) -> bool:
        """True when nothing is configured — resolution can treat it as ``None``."""
        return (
            self.instructions is None
            and self.personality is None
            and self.greeting is None
            and self.conversation_workflow is None
            and not self.enabled_tools
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

    visibility = "internal" if raw.get("visibility") == "internal" else "public"
    config = AgentConfig(
        instructions=instructions,
        personality=_clean_str(raw.get("personality")),
        greeting=_clean_str(raw.get("greeting")),
        conversation_workflow=parse_workflow(raw.get("conversation_workflow")),
        enabled_tools=_parse_enabled_tools(raw.get("tool_config")),
        visibility=visibility,
    )
    return None if config.is_empty else config


def _parse_enabled_tools(raw: Any) -> list[EnabledTool]:
    """Parse ``tool_config.enabledTools`` tolerantly. Each entry needs a string
    ``toolId``; ``enabled`` defaults true, ``authLevel`` defaults ``"none"``, ``config``
    is an optional dict. Malformed entries are dropped; a bad/missing shape → ``[]``
    (→ full tool set)."""
    if not isinstance(raw, dict):
        return []
    entries = raw.get("enabledTools")
    if not isinstance(entries, list):
        return []
    out: list[EnabledTool] = []
    for item in entries:
        if not isinstance(item, dict):
            continue
        tool_id = _clean_str(item.get("toolId"))
        if tool_id is None:
            continue
        enabled = item.get("enabled")
        cfg = item.get("config")
        out.append(
            EnabledTool(
                tool_id=tool_id,
                enabled=enabled if isinstance(enabled, bool) else True,
                auth_level=_clean_str(item.get("authLevel")) or "none",
                config=cfg if isinstance(cfg, dict) else {},
            )
        )
    return out


def filter_tools(tools: list[Any], config: AgentConfig | None) -> list[Any]:
    """Restrict ``tools`` to the agent's allow-list (``enabled=true`` ``tool_id``s,
    matched by ``.name``). An empty ``enabledTools`` or ``None`` config returns
    ``tools`` unchanged (un-configured agents keep the full server tool set); unknown
    tool_ids are ignored. Mirrors the monorepo ``AgentToolConfig`` semantics."""
    if config is None or not config.enabled_tools:
        return tools
    allowed = {e.tool_id for e in config.enabled_tools if e.enabled}
    return [t for t in tools if t.name in allowed]


#: Reserved key under which a tool's per-agent ``config`` dict is delivered inside the
#: execution ``arguments`` (the reference server hands pre-built tools, so config rides
#: with the call rather than being baked in at tool creation like the monorepo registry).
CONFIG_ARG_KEY = "__tool_config__"


class SessionAuthenticator(ABC):
    """Decides whether a conversation's user has verified their identity — the seam a
    host wires OTP/JWT verification behind. Fail-closed by default: an unwired server
    treats every session as unauthenticated, so ``end_user`` tools on public agents
    stay gated until a host proves identity."""

    @abstractmethod
    async def is_authenticated(self, conversation_id: str) -> bool: ...


class NoSessionAuthenticator(SessionAuthenticator):
    """The default: no identity is ever verified (fail-closed)."""

    async def is_authenticated(self, conversation_id: str) -> bool:
        return False


@dataclass
class OtpRefusal:
    """A mutable slot recording an ``end_user`` tool this turn's gate refused for
    lack of a verified session — the Python analog of the Rust gate's
    ``Arc<Mutex<Option<String>>>``. The dispatcher shares one across a turn's gated
    tools and reads :attr:`refused_tool` after the turn to decide whether to offer
    the OTP flow. An ``admin`` refusal is never recorded here (no OTP can satisfy
    it)."""

    refused_tool: str | None = None


class _GatedTool:
    """Wraps a :class:`Tool`, enforcing the per-agent ``authLevel`` at execution and
    delivering the entry's ``config`` to the inner tool. Structurally a ``Tool``
    (delegates name/description/parameters). Mirrors ``tool-execution.ts``.

    ``session_authenticated`` is the turn's resolved identity bit (the session's
    OTP-verified state OR'd with the host :class:`SessionAuthenticator` seam),
    computed once by the dispatcher before the turn — the Python analog of the Rust
    gate's ``session_authenticated`` field."""

    def __init__(
        self,
        inner: Any,
        entry: EnabledTool,
        visibility: str,
        session_authenticated: bool,
        recorder: OtpRefusal | None = None,
    ) -> None:
        self.inner = inner
        self.name = inner.name
        self.description = inner.description
        self.parameters = inner.parameters
        self._entry = entry
        self._visibility = visibility
        self._session_authenticated = session_authenticated
        self._recorder = recorder

    async def execute(self, arguments: dict[str, Any]) -> str:
        auth_level = self._entry.auth_level
        # Gate only when a non-trivial auth level is set AND the tool opts in via
        # `supports_auth_requirement` (default False) — faithful to the reference.
        if auth_level != "none" and getattr(self.inner, "supports_auth_requirement", False):
            if auth_level == "admin" and self._visibility == "public":
                # An admin refusal is not OTP-remediable → never recorded for OTP.
                return f"Tool '{self.name}' requires admin authentication and is not available on public-facing agents."
            # Internal agents auto-satisfy end_user/admin (the session is already authed).
            # Public agents must verify identity for end_user before the tool runs.
            if self._visibility != "internal" and not self._session_authenticated:
                # Record an `end_user` refusal so the server can offer the OTP flow
                # after the turn. Only `end_user` is OTP-remediable (mirrors the Rust
                # gate, which records only AuthLevel::EndUser).
                if auth_level == "end_user" and self._recorder is not None:
                    self._recorder.refused_tool = self.name
                return f"Tool '{self.name}' requires identity verification before it can run. Ask the user to verify their identity with a one-time code."
        # Deliver the per-tool config (only when set → behavior unchanged otherwise).
        if self._entry.config:
            arguments = {**arguments, CONFIG_ARG_KEY: self._entry.config}
        return await self.inner.execute(arguments)


def gate_tools(
    tools: list[Any],
    config: AgentConfig | None,
    session_authenticated: bool,
    recorder: OtpRefusal | None = None,
) -> list[Any]:
    """Wrap each tool that has an ``authLevel`` to enforce or per-tool ``config`` to
    deliver. Tools with neither pass through untouched, so an un-configured agent's
    tools are unchanged. ``session_authenticated`` is the turn's resolved identity
    bit; ``recorder`` (when given) captures an ``end_user`` refusal for the post-turn
    OTP offer."""
    if config is None or not config.enabled_tools:
        return tools
    by_id = {e.tool_id: e for e in config.enabled_tools}
    out: list[Any] = []
    for tool in tools:
        entry = by_id.get(tool.name)
        if entry is None or (entry.auth_level == "none" and not entry.config):
            out.append(tool)
        else:
            out.append(_GatedTool(tool, entry, config.visibility, session_authenticated, recorder))
    return out


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
