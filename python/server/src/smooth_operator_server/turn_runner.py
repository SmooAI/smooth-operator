"""The streaming, memory-carrying agent runner used by the WS service.

Drives one ``send_message`` turn: replay the conversation's prior history as
memory, run the in-process :class:`SmoothAgent` in streaming mode, map each engine
stream event onto a protocol event (``stream_token`` per text delta,
``stream_chunk`` per tool call / result), persist the reply, and return the result.

The Python analog of the C# ``TurnRunner`` and the Rust ``run_streaming_turn``.
ACL-filtered retrieval, the rerank stage, and tool/HITL gating are seams left open
for later phases (the MVP wires the knowledge base straight through).
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import math
import os
from dataclasses import dataclass, field, fields
from typing import Any, Callable

from smooth_operator_core import (
    AgentExecutor,
    AgentOptions,
    DoneEvent,
    HumanApprovalRequest,
    HumanApprovalResponse,
    InProcessExecutor,
    Knowledge,
    SmoothAgent,
    SmoothAgentThread,
    TextEvent,
    ToolCallEvent,
    ToolResultEvent,
)

from . import protocol, telemetry
from .agent_config import AgentConfig, filter_tools
from .confirmation import ConfirmationRegistry
from .extensions import build_extension_host
from .interaction import InteractionRegistry, PendingInteractions
from .interaction_tools import build_interaction_tools
from .model_info import model_output_ceiling
from .session_store import MessageDirection, SessionStore
from .workflow import (
    WORKFLOW_JUDGE_MODEL,
    judge_workflow_step,
    next_step,
    render_workflow_prompt_section,
    resolve_current_step,
)

#: Max prior turns replayed into the thread for memory (bounds context growth).
MAX_PRIOR_MESSAGES = 50

#: The engine's default model when the server pins none (matches
#: ``AgentOptions.model`` in smooth-operator-core). Used to look up the output
#: ceiling for the model the turn will actually send.
DEFAULT_MODEL = "claude-haiku-4-5"

#: Per-call ``max_tokens`` sent to the gateway. Raised from the old chat-widget
#: default of 512 — that STARVES reasoning models (they exhaust the budget on
#: reasoning and return empty). Mirrors the Rust server's ``DEFAULT_MAX_TOKENS``
#: 512→8192 (EPIC th-1cc9fa). The engine still clamps this DOWN to the model's real
#: output ceiling (:func:`model_output_ceiling`), so raising it is safe.
DEFAULT_MAX_TOKENS = 8192

#: Agent-loop iteration cap per turn. Raised from 6 — a tool-using reasoning turn
#: routinely needs more than six model round-trips. Mirrors the Rust server's
#: ``DEFAULT_MAX_ITERATIONS`` 6→20 (EPIC th-1cc9fa).
DEFAULT_MAX_ITERATIONS = 20

#: Whether the installed smooth-operator-core supports the ceiling clamp field.
#: The server pins the PUBLISHED core (see ``pyproject.toml``); a core predating the
#: clamp has no ``model_max_output`` on ``AgentOptions``, so passing it would raise
#: ``TypeError``. Feature-detect: thread the ceiling only when the field exists, so
#: this stays green on the pinned core and activates automatically once a core with
#: the clamp is released. (Release-ordering: core PR ships + publishes first.)
_CORE_SUPPORTS_CEILING = any(f.name == "model_max_output" for f in fields(AgentOptions))

#: Whether the installed smooth-operator-core supports tool-call lifecycle hooks
#: (``AgentOptions.tool_hooks`` / the ``ToolHook`` seam). Same release-ordering as
#: the ceiling clamp: the server pins the PUBLISHED core, so a core predating hooks
#: has no ``tool_hooks`` field and passing it would raise ``TypeError``. Thread hooks
#: through only when the field exists — stays green on the pinned core, activates
#: automatically once a core with the seam is released. (Core PR ships + publishes
#: first.)
_CORE_SUPPORTS_HOOKS = any(f.name == "tool_hooks" for f in fields(AgentOptions))

#: Top-K knowledge hits surfaced as auto-context citations (what grounded the
#: answer). Matches the engine's auto-context injection and the TS/C#/Rust servers.
AUTO_CONTEXT_LIMIT = 3

#: Max chars of a hit's content carried in a citation snippet (matches the TS
#: ``CITATION_SNIPPET_MAX_CHARS`` / the Rust/C# truncation).
CITATION_SNIPPET_MAX_CHARS = 280

#: Default system prompt — ground answers in the knowledge base. Mirrors the
#: C#/Rust runner prompts.
DEFAULT_SYSTEM_PROMPT = (
    "You are a helpful customer support agent. Answer using only the knowledge "
    "provided to you; if it is not there, say you don't know."
)

#: ``max_tokens`` for the fast-model preamble — one short sentence. Mirrors the
#: Rust runner's ``PREAMBLE_MAX_TOKENS``. Pearl th-ce3888.
PREAMBLE_MAX_TOKENS = 64

#: System prompt for the fast-model preamble (see ``SMOOTH_AGENT_PREAMBLE_MODEL``).
#: One short present-tense sentence describing intent — no answer (it is generated
#: WITHOUT any tool result), no greeting, no promises. Byte-for-byte the Rust
#: runner's ``PREAMBLE_SYSTEM_PROMPT``.
PREAMBLE_SYSTEM_PROMPT = (
    "You are the assistant's voice while it works. "
    "In ONE short present-tense sentence (max ~12 words), tell the user what you're about to do to help with their message. "
    "Do NOT answer the question, do NOT greet, do NOT promise a specific result or outcome. "
    'Example: "Let me pull up your recent conversations." '
    "Reply with only that sentence — no quotes, no preamble, no markdown."
)

logger = logging.getLogger(__name__)


#: Env var a deployment sets to run turns on a durable backend (ADR-030) instead of
#: in-process. Unset — the default — is the in-process executor, a verbatim
#: delegation to ``SmoothAgent.run_stream``, so a deployment that never sets this
#: behaves exactly as it did before the seam existed. Mirrors the Rust server's
#: ``DURABLE_EXECUTOR_ENV`` (``runner.rs``).
DURABLE_EXECUTOR_ENV = "SMOOTH_AGENT_DURABLE_EXECUTOR"


def durable_requested(value: str | None) -> bool:
    """Whether ``value`` opts into durable execution.

    Off unless explicitly asked — an unset, empty, or unrecognized value stays off.
    Separated from the env read so the parse is testable without mutating
    process-global state (mirrors the Rust ``durable_requested``)."""
    return (value or "").strip().lower() in {"1", "true", "on", "yes"}


def select_turn_executor(injected: AgentExecutor | None, env_value: str | None = None) -> AgentExecutor:
    """The executor a turn runs on — the one place a durable backend plugs in.

    A durable backend is dependency-**injected** (``TurnRunner(executor=...)``), so
    this module needs no hard dependency on the Temporal package: the seam works with
    the injected executor as an opaque ``AgentExecutor``. Selection is env-gated to
    match the parity spec — the injected durable backend is used only when
    ``SMOOTH_AGENT_DURABLE_EXECUTOR`` opts in; otherwise (and whenever nothing is
    injected) this resolves to the in-process executor, a verbatim delegation to
    ``SmoothAgent.run_stream``. Asking for durable mode without supplying an executor
    warns and falls back, rather than silently pretending a turn is durable.

    (This differs deliberately from the Rust ``turn_executor``, where an injected
    executor wins unconditionally: here the env var is the explicit opt-in, so an
    injected backend can never silently take over a deployment that didn't ask.)"""
    if env_value is None:
        env_value = os.environ.get(DURABLE_EXECUTOR_ENV)
    if durable_requested(env_value):
        if injected is not None:
            return injected
        logger.warning(
            "%s requested but no durable executor was supplied on the turn; running the turn in-process",
            DURABLE_EXECUTOR_ENV,
        )
    return InProcessExecutor()


def preamble_model() -> str | None:
    """The fast model id for the parallel preamble, or ``None`` when the feature is
    off. Unset / empty / whitespace ⇒ off (no extra LLM call, behavior unchanged) —
    the same contract as the Rust server's ``SMOOTH_AGENT_PREAMBLE_MODEL``."""
    model = (os.environ.get("SMOOTH_AGENT_PREAMBLE_MODEL") or "").strip()
    return model or None


#: A sink the runner emits ready-to-send protocol event dicts through (the
#: connection forwards them over the socket). Sync send, like the Rust ``sink``.
Sink = Callable[[dict[str, Any]], None]


@dataclass
class TurnContext:
    """Per-turn host context — the Python analog of the Rust server's
    ``ToolProviderContext`` for the file-transfer surface (``send_message.files[]`` +
    the ``send_file`` directive convention, spec PR #342).

    Carries the turn's attachments and the directive sink so a host tool can see what
    the turn brought in and hand a client-side directive back out:

    * :attr:`images` — the multimodal image attachments; the runner maps these onto
      the model's user message (OpenAI ``image_url`` content parts).
    * :attr:`files` — non-image file attachments, surfaced here for a host tool to
      land into the agent's workspace. NOT sent to the model.
    * :attr:`directive` — the **directive sink**: a host tool writes a dict here
      (via :meth:`set_directive`); the runner drains it after the turn onto
      :attr:`TurnResult.directive`, which the dispatcher attaches to
      ``eventual_response.directive``. ``None`` (the default) ⇒ no host directive.

    A host tool that needs any of this is constructed per turn closing over the
    context, exactly as the Rust provider builds tools from ``ToolProviderContext``.
    """

    images: list[dict[str, Any]] = field(default_factory=list)
    files: list[dict[str, Any]] = field(default_factory=list)
    directive: dict[str, Any] | None = None

    def set_directive(self, value: dict[str, Any]) -> None:
        """Write a client-side directive onto the sink. Last-write-wins across the
        turn (mirrors the Rust ``directive_sink`` ``Mutex<Value>`` semantics), so a
        tool delivering several files should emit them in ONE directive, not one per
        call."""
        self.directive = value


@dataclass(frozen=True)
class TurnResult:
    """What a completed turn produced (the analog of the Rust/C# ``TurnResult``)."""

    reply: str
    message_id: str
    citations: list[dict[str, Any]] = field(default_factory=list)
    #: Client-side directive drained from the turn's :class:`TurnContext` sink (a host
    #: tool wrote it — e.g. ``send_file``). ``None`` ⇒ no directive; the dispatcher
    #: then omits ``eventual_response.directive`` (back-compat).
    directive: dict[str, Any] | None = None
    #: Token accounting + cost captured from the engine's terminal ``DoneEvent``
    #: (``{costUsd, promptTokens, completionTokens}``). ``None`` when the stream
    #: produced no terminal event; the dispatcher then omits
    #: ``eventual_response.usage`` exactly like the Rust reference.
    usage: dict[str, Any] | None = None


class TurnRunner:
    """Runs one knowledge-grounded streaming turn and emits protocol events as it
    goes. Constructed per turn, bound to the connection's chat client + store."""

    def __init__(
        self,
        chat_client: Any,
        store: SessionStore,
        knowledge: Knowledge | None = None,
        system_prompt: str | None = None,
        model: str | None = None,
        tools: list[Any] | None = None,
        confirm_tools: list[str] | None = None,
        confirmations: ConfirmationRegistry | None = None,
        agent_config: AgentConfig | None = None,
        judge_model: str | None = None,
        tool_hooks: list[Any] | None = None,
        org_id: str | None = None,
        executor: AgentExecutor | None = None,
        interactions: InteractionRegistry | None = None,
        interaction_pending: PendingInteractions | None = None,
        capabilities: list[str] | None = None,
    ) -> None:
        self._chat_client = chat_client
        self._store = store
        #: The executor the turn's engine runs on. Selected once here from the
        #: injected backend + ``SMOOTH_AGENT_DURABLE_EXECUTOR`` (see
        #: :func:`select_turn_executor`): a durable backend arrives by injection, and
        #: with nothing injected (the default) this is the in-process executor, a
        #: verbatim delegation to ``SmoothAgent.run_stream`` — behavior unchanged.
        self._executor = select_turn_executor(executor)
        self._knowledge = knowledge
        self._system_prompt = system_prompt or DEFAULT_SYSTEM_PROMPT
        #: Resolved per-agent config (instructions / workflow / persona). ``None`` →
        #: the server-wide default prompt drives the turn (behavior unchanged).
        self._agent_config = agent_config
        #: Fast/cheap model for the post-turn workflow judge (default haiku-tier).
        self._judge_model = judge_model or WORKFLOW_JUDGE_MODEL
        self._model = model
        self._tools = tools or []
        #: Tool-name substrings that require human approval before they run (empty →
        #: HITL off, behavior unchanged). Matched by substring like the Rust hook.
        self._confirm_tools = confirm_tools or []
        #: The session-keyed pending-confirmation registry the gate parks on.
        self._confirmations = confirmations
        #: Tool-call lifecycle hooks (``ToolHook``) installed on every turn's engine —
        #: surveillance / redaction (e.g. a Narc-style secret scrubber) that sees every
        #: tool call. Empty (the default) ⇒ no hooks. Threaded into ``AgentOptions``
        #: only when the installed core supports the seam (see ``_CORE_SUPPORTS_HOOKS``).
        self._tool_hooks = tool_hooks or []
        #: The owning org, recorded as ``smooai.org_id`` on the turn's ``gen_ai.chat``
        #: span so the observability studio groups turns by org (mirrors the Rust
        #: runner's ``org_id`` span field). ``None`` ⇒ the attribute is omitted.
        self._org_id = org_id
        #: Rich Interactions catalog + the session-keyed park registry (shared with the
        #: dispatcher's ``submit_interaction`` handler). When both are wired, this turn
        #: registers the per-kind ``request_<kind>`` raise tools + the generic
        #: ``submit_interaction`` fallback tool. ``None`` ⇒ no interaction tools (behavior
        #: unchanged).
        self._interactions = interactions
        self._interaction_pending = interaction_pending
        #: The client render capabilities this session declared in ``supports`` — gates
        #: whether each kind's raise tool parks (rich) or degrades (fallback).
        self._capabilities = set(capabilities or [])

    def _is_gated(self, tool_name: str) -> bool:
        """True when ``tool_name`` matches a confirmation-gated pattern (substring,
        like the Rust hook). Only meaningful when a confirmation registry is wired."""
        if self._confirmations is None:
            return False
        return any(pattern in tool_name for pattern in self._confirm_tools)

    def _is_interaction_raise(self, tool_name: str) -> bool:
        """True when ``tool_name`` is one of this turn's ``request_<kind>`` raise tools.
        Their toolCall chunk is deferred out of the stream loop and re-emitted by the
        tool itself — after ``interaction_required`` on the park path."""
        if self._interactions is None:
            return False
        return any(kind.tool_schema()["name"] == tool_name for kind in self._interactions.kinds())

    async def run(
        self,
        conversation_id: str,
        request_id: str,
        user_message: str,
        sink: Sink,
        session_id: str | None = None,
        context: TurnContext | None = None,
    ) -> TurnResult:
        # 0. Auto-context citations (what grounded the answer). Mirrors the TS
        #    server's citation build / the Rust auto_sources / the C# citation build.
        #    The engine's `query` is the same retriever the agent injects from, so the
        #    citations match the grounding the model actually saw. Built BEFORE the
        #    agent runs (the query is independent of generation).
        citations = self._build_citations(user_message)

        # 0b. Load prior history up front — it drives both the thread replay (memory)
        #     and whether this is the first turn (greeting awareness).
        prior_messages = await self._store.list_messages(conversation_id, MAX_PRIOR_MESSAGES)

        # 0c. Resolve the workflow-step pointer for this conversation (SMOODEV-590).
        #     The judge advances it after the turn; here it selects which step's
        #     intent/criteria get rendered into the system prompt.
        workflow = self._agent_config.conversation_workflow if self._agent_config else None
        current_step_id = await self._store.get_current_step_id(conversation_id) if workflow else None

        # 0d. SEP — build the per-turn extension host (default-deny SMOOTH_EXTENSIONS_ALLOW;
        #     None + zero overhead when unset). Its eager tools join the agent's tool set,
        #     flowing through the SAME per-agent enabled_tools filter the built-ins got
        #     (filter_tools by tool name), so an allow-list drops an ext tool exactly like
        #     a built-in. The ext host's ui/confirm bridges onto the confirmation frame via
        #     the ConfirmUiProvider (see extensions.py) — same registry the write HITL uses.
        confirm_session = session_id or conversation_id
        ext_turn = None
        if self._confirmations is not None:
            ext_turn = await build_extension_host(confirm_session, request_id, sink, self._confirmations)
        agent_tools = list(self._tools)
        if ext_turn is not None:
            agent_tools.extend(filter_tools(ext_turn.host.tools(), self._agent_config))

        # Rich Interactions: register this turn's per-kind `request_<kind>` raise tools
        # (rich when the session declared the kind's capability, else conversational
        # fallback) + the generic `submit_interaction` tool. Framework tools, appended
        # raw (not through the agent allow-list) so they are always available, mirroring
        # the Rust runner's unconditional registration. `interaction_required` events
        # emit through this connection's sink; a rich raise parks on
        # `_interaction_pending` until the dispatcher's `submit_interaction` resolves it.
        if self._interactions is not None and self._interaction_pending is not None:
            agent_tools.extend(
                build_interaction_tools(
                    self._interactions,
                    self._capabilities,
                    confirm_session,
                    request_id,
                    sink,
                    self._interaction_pending,
                    self._store,
                )
            )

        # 1. Build the agent. The knowledge base (when present) auto-injects the
        #    top hits into the system prompt — the engine handles retrieval + rerank
        #    internally, mirroring the C# `new SmoothAgent(..., Knowledge = ...)`.
        options_kwargs: dict[str, Any] = {
            "instructions": self._assemble_system_prompt(current_step_id, is_first_turn=not prior_messages),
            "knowledge": self._knowledge,
            # Raised, anti-starvation sizing (see the module constants). The engine
            # clamps max_tokens DOWN to the model's real output ceiling below.
            "max_tokens": DEFAULT_MAX_TOKENS,
            "max_iterations": DEFAULT_MAX_ITERATIONS,
        }
        if agent_tools:
            options_kwargs["tools"] = agent_tools
        if self._model is not None:
            options_kwargs["model"] = self._model

        # Install tool-call lifecycle hooks (ToolHook) on the per-turn engine. Every
        # hook's pre_call gates each tool (raise → block) and its post_call may redact
        # the result in place. Feature-detected against the pinned core (see
        # `_CORE_SUPPORTS_HOOKS`); empty hooks ⇒ nothing threaded, behavior unchanged.
        if self._tool_hooks and _CORE_SUPPORTS_HOOKS:
            options_kwargs["tool_hooks"] = self._tool_hooks

        # Clamp max_tokens to what the resolved model can physically emit: look up
        # its output ceiling from the gateway (best-effort; None ⇒ unclamped) and
        # thread it into the engine's clamp. Feature-detected against the pinned
        # core (see `_CORE_SUPPORTS_CEILING`).
        if _CORE_SUPPORTS_CEILING:
            ceiling = await model_output_ceiling(self._model or DEFAULT_MODEL)
            if ceiling is not None:
                options_kwargs["model_max_output"] = ceiling

        # Write-confirmation HITL: when configured with tool patterns AND a registry
        # is present, install a HumanGate that parks the turn before a gated tool
        # runs (emit `write_confirmation_required`, await the client's verdict via
        # the session-keyed registry). With no patterns (the default) no gate is
        # installed → no tool ever parks → behavior identical to before HITL. The
        # gate keys its pending future by `session_id`, so a `confirm_tool_action`
        # frame (also keyed by sessionId) routes back here. (`confirm_session` is
        # resolved above, where the extension host is built.)
        if self._confirm_tools and self._confirmations is not None:
            patterns = self._confirm_tools
            registry = self._confirmations

            def _requires_approval(tool_name: str, _args: dict[str, Any]) -> bool:
                return any(pattern in tool_name for pattern in patterns)

            async def _gate(req: HumanApprovalRequest) -> HumanApprovalResponse:
                # Park: register a fresh future, emit the confirmation event, then
                # await the client's `confirm_tool_action`. The toolId is the tool
                # name (one tool parks at a time — a stable correlation key).
                #
                # Event ORDER matters for cross-language parity: the reference (Rust)
                # server emits `write_confirmation_required` BEFORE the gated tool's
                # `stream_chunk(toolCall)`. The engine, however, yields the
                # ToolCallEvent before consulting the gate — so the stream loop
                # DEFERS a gated tool's `stream_chunk` (see `_is_gated`) and we emit
                # it HERE, right after the confirmation prompt, to match.
                future = registry.register(confirm_session)
                sink(protocol.write_confirmation_required(request_id, req.tool_name, req.prompt))
                sink(
                    protocol.stream_chunk(
                        request_id, req.tool_name, _tool_call_state_from(req.tool_name, req.arguments)
                    )
                )
                approved = await future
                if approved:
                    return HumanApprovalResponse.approve()
                return HumanApprovalResponse.deny("user rejected the action")

            from smooth_operator_core import DelegateHumanGate

            options_kwargs["human_gate"] = DelegateHumanGate(_gate)
            options_kwargs["requires_approval"] = _requires_approval

        agent = SmoothAgent(self._chat_client, AgentOptions(**options_kwargs))

        # 2. Replay prior history as the thread (before persisting this turn's
        #    inbound), so the model sees turn 1 when answering turn 2. Mirrors the
        #    Rust `load_prior_messages` + the C# thread replay.
        thread = SmoothAgentThread(id=conversation_id)
        for message in prior_messages:
            role = "assistant" if message.direction == MessageDirection.OUTBOUND else "user"
            thread.add({"role": role, "content": message.text})

        # 4. Stream the turn: a stream_token per text delta, a stream_chunk per tool
        #    call / tool result (mirrors the Rust runner translating the engine's
        #    AgentEvent stream). The terminal DoneEvent carries the final
        #    AgentRunResponse, whose `text` is authoritative for the reply. The inbound
        #    persist rides inside the try so the extension host is always torn down
        #    (finally) once it has been spawned above.
        reply_parts: list[str] = []
        final_text: str | None = None
        final_usage: dict[str, Any] | None = None

        # Optional fast-model preamble (pearl th-ce3888). When
        # `SMOOTH_AGENT_PREAMBLE_MODEL` is set, a small fast model runs CONCURRENTLY
        # with the agent loop and emits ONE ephemeral `stream_preamble` sentence,
        # covering the reasoning model's time-to-first-token. It uses this
        # connection's chat client (same gateway/key) with only the model + a tight
        # token cap overridden. `answer_started` is flipped on the first real answer
        # token so a slow preamble can never pop in AFTER the answer began. Unset ⇒
        # off: no task, no LLM call, behavior unchanged.
        answer_started = asyncio.Event()
        preamble_task: asyncio.Task[None] | None = None
        fast_model = preamble_model()
        if fast_model is not None and self._chat_client is not None:
            preamble_task = asyncio.create_task(
                self._run_preamble(fast_model, request_id, user_message, sink, answer_started)
            )

        # Multimodal turn: when the turn carried image attachments, hand the engine's
        # user message an OpenAI content-parts list (text + `image_url` parts) instead
        # of the bare string, so the model actually sees the images. Mirrors the Rust
        # server's `with_user_images`, but attached in-tree here (the pinned published
        # core takes `run_stream(message)` and drops it straight onto the user message,
        # so no core release is needed).
        #
        # Guard (pinned-core limitation): that core reuses the SAME `message` for its
        # knowledge/memory retrieval query, which tokenizes it as a string — a content
        # list would crash `kb.query`. So attach the multimodal payload only when no
        # knowledge base is wired (the LocalServer/daemon flavor, and every images test).
        # With knowledge wired we fall back to text-only rather than crash — fail-soft,
        # matching the `images` schema. (Clean fix lands when the core grows a dedicated
        # user-images seam; feature-detect it then, as with `_CORE_SUPPORTS_*`.)
        model_message: Any = user_message
        if context is not None and context.images and self._knowledge is None:
            model_message = _build_user_content(user_message, context.images)

        # OpenTelemetry GenAI turn span (mirrors the Rust `run_streaming_turn`
        # `gen_ai.chat` span). Wraps the whole engine run so the `gen_ai.tool` child
        # spans nest under it and token usage is recorded onto it. `smooai.org_id`
        # matches the monorepo TS chat handler so the studio groups by org. A no-op
        # (zero cost) until `init_telemetry` installs a provider.
        span_attrs: dict[str, Any] = {
            telemetry.GEN_AI_SYSTEM: telemetry.SYSTEM_NAME,
            telemetry.GEN_AI_OPERATION_NAME: telemetry.OPERATION_CHAT,
            telemetry.GEN_AI_REQUEST_MODEL: self._model or DEFAULT_MODEL,
            telemetry.GEN_AI_CONVERSATION_ID: conversation_id,
            telemetry.GEN_AI_AGENT_NAME: telemetry.AGENT_NAME,
        }
        if self._org_id:
            span_attrs[telemetry.SMOOAI_ORG_ID] = self._org_id

        try:
            with telemetry.tracer().start_as_current_span(telemetry.SPAN_CHAT, attributes=span_attrs) as turn_span:
                # 3. Persist the inbound user message.
                await self._store.append_message(conversation_id, MessageDirection.INBOUND, user_message)

                # Run the turn through the executor seam (default in-process, a durable
                # backend when injected + opted in), not `agent.run_stream` directly —
                # the one place a durable backend plugs in (mirrors the Rust runner).
                async for event in self._executor.execute_streaming(agent, model_message, thread=thread):
                    if isinstance(event, TextEvent):
                        if event.text:
                            # Close the preamble window BEFORE the first answer token
                            # goes out, so a preamble resolving concurrently is dropped.
                            answer_started.set()
                            reply_parts.append(event.text)
                            sink(protocol.stream_token(request_id, event.text))
                    elif isinstance(event, ToolCallEvent):
                        # One `gen_ai.tool` child span per tool call, carrying the tool
                        # name + redacted arguments (mirrors the Rust runner's per-tool
                        # child span). Emitted for gated tools too — the span is
                        # independent of the deferred wire chunk below.
                        _emit_tool_span(event, conversation_id, self._org_id)
                        # DEFER a parking tool's toolCall chunk: it is emitted from the
                        # park path AFTER `write_confirmation_required` /
                        # `interaction_required`, so the wire order matches the reference
                        # (Rust) server. Ungated tools emit their chunk inline as before.
                        if self._is_gated(event.name) or self._is_interaction_raise(event.name):
                            continue
                        sink(protocol.stream_chunk(request_id, event.name, _tool_call_state(event)))
                    elif isinstance(event, ToolResultEvent):
                        sink(protocol.stream_chunk(request_id, event.name, _tool_result_state(event)))
                    elif isinstance(event, DoneEvent):
                        final_text = event.response.text
                        # The terminal event is not re-emitted as a stream event, but its
                        # accumulated cost + token counts become `eventual_response.usage`.
                        final_usage = {
                            "costUsd": event.response.cost_usd,
                            "promptTokens": event.response.usage.prompt_tokens,
                            "completionTokens": event.response.usage.completion_tokens,
                        }
                        _record_turn_usage(
                            turn_span,
                            event.response.usage.prompt_tokens,
                            event.response.usage.completion_tokens,
                            event.response.cost_usd,
                        )
        finally:
            # Turn over: the preamble window is closed for good. Cancel and reap the
            # task so a still-in-flight preamble can neither emit late nor linger as
            # a pending task at shutdown. No-op when the feature is off.
            if preamble_task is not None:
                answer_started.set()
                preamble_task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await preamble_task
            # Drop any lingering pending confirmation so a stale entry
            # can't mis-route a later `confirm_tool_action` (mirrors the Rust
            # `(cfg.clear)(session_id)` at turn end). No-op when HITL is off.
            if self._confirmations is not None:
                self._confirmations.clear(confirm_session)
            # Drop any interaction still parked (mirrors the Rust `(cfg.clear)` at turn
            # end): a raise tool awaiting at teardown unblocks `no_response`, and a stale
            # registration can't mis-route a later `submit_interaction`. No-op when off.
            if self._interaction_pending is not None:
                self._interaction_pending.clear(confirm_session)
            # SEP — stop the extension subprocesses this turn spawned (and clear any
            # ui/confirm still parked). No-op when no host was built (default deny).
            if ext_turn is not None:
                await ext_turn.teardown()

        # The DoneEvent's text wins (it's the engine's authoritative final), falling
        # back to the concatenated streamed deltas if it's empty.
        reply = final_text if final_text else "".join(reply_parts)

        # 5. Persist the outbound reply.
        outbound = await self._store.append_message(conversation_id, MessageDirection.OUTBOUND, reply)

        # 6. Post-turn workflow judge (SMOODEV-590): decide whether the current step's
        #    criteria were met this turn and advance the conversation's pointer. No-op
        #    when no workflow is configured. Failure-tolerant — the judge stays on the
        #    current step on any error, so a judge outage never freezes the flow.
        if workflow is not None:
            await self._advance_workflow(conversation_id, workflow, current_step_id, user_message, reply)

        # 7. Drain the directive sink (mirrors the Rust runner's post-turn drain). A
        #    host tool that ran this turn may have written a client-side directive onto
        #    the context (e.g. `send_file`); `None` ⇒ none was written, so the
        #    dispatcher omits `eventual_response.directive` (back-compat).
        directive = context.directive if context is not None else None

        return TurnResult(
            reply=reply,
            message_id=outbound.id,
            citations=citations,
            directive=directive,
            usage=final_usage,
        )

    async def _run_preamble(
        self,
        model: str,
        request_id: str,
        user_message: str,
        sink: Sink,
        answer_started: asyncio.Event,
    ) -> None:
        """One-shot fast-model preamble, run as a task alongside the agent loop.

        Best-effort by construction: any failure (gateway error, timeout, malformed
        response) is logged at debug and swallowed — it never surfaces an ``error``
        event and never fails the turn. Cancellation is NOT swallowed: the turn's
        ``finally`` cancels this task, and that must propagate. The ``answer_started``
        guard is checked immediately before emitting, so a preamble that resolves
        after the real answer began emits nothing. Pearl th-ce3888."""
        try:
            response = await self._chat_client.chat.completions.create(
                model=model,
                max_tokens=PREAMBLE_MAX_TOKENS,
                messages=[
                    {"role": "system", "content": PREAMBLE_SYSTEM_PROMPT},
                    # The user's message is the ONLY user-role content — the preamble
                    # is generated without history and without tool results, so it can
                    # only describe intent, never answer.
                    {"role": "user", "content": user_message},
                ],
            )
            token = (response.choices[0].message.content or "").strip()
            if token and not answer_started.is_set():
                sink(protocol.stream_preamble(request_id, token))
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001 — best-effort by design
            logger.debug("preamble generation failed (ignored): %s", exc)

    def _assemble_system_prompt(self, current_step_id: str | None, *, is_first_turn: bool) -> str:
        """Assemble the turn's system prompt from the per-agent config, falling back
        to the server-wide default.

        Precedence for the base body: the agent's ``instructions`` prompt →
        the server-wide ``system_prompt`` → :data:`DEFAULT_SYSTEM_PROMPT`. A
        personality note and a first-turn greeting seed are appended when the agent
        configured them, and the current workflow step (if any) is rendered last so
        the model sees the active intent/criteria. With no agent config the result
        is exactly the server-wide prompt (behavior unchanged)."""
        config = self._agent_config
        base = (config.instructions if config and config.instructions else None) or self._system_prompt

        sections = [base]
        if config is not None:
            if config.personality:
                sections.append(f"<Personality>\n{config.personality}\n</Personality>")
            if is_first_turn and config.greeting:
                sections.append(
                    "<GreetingAwareness>\nThis is your first reply in this conversation. Open with a natural, "
                    f'brief variant of: "{config.greeting}" — then address the user\'s message in the same reply. '
                    "Do NOT repeat the greeting verbatim, and do not reintroduce yourself later.\n</GreetingAwareness>"
                )
            workflow_section = render_workflow_prompt_section(config.conversation_workflow, current_step_id)
            if workflow_section:
                sections.append(workflow_section)

        return "\n\n".join(sections)

    async def _advance_workflow(
        self,
        conversation_id: str,
        workflow: Any,
        current_step_id: str | None,
        user_message: str,
        reply: str,
    ) -> None:
        """Judge the current step and persist the advanced pointer.

        On a ``yes`` verdict the pointer moves to :func:`next_step` (explicit ``next``
        → sequential → terminal). Any other verdict (``no``/``maybe``/``skipped``) or
        a judge failure leaves the conversation on the current step, so it renders
        the same step next turn."""
        current = resolve_current_step(workflow, current_step_id)
        if current is None:
            return
        verdict = await judge_workflow_step(
            self._chat_client, workflow, current, user_message, reply, model=self._judge_model
        )
        if verdict == "yes":
            advance = next_step(workflow, current)
            resolved = advance.id if advance is not None else current.id
        else:
            resolved = current.id
        # Persist the pointer (matches the TS judge, which always writes current.id),
        # skipping the write only when it already holds this exact value.
        if resolved != current_step_id:
            await self._store.set_current_step_id(conversation_id, resolved)

    def _build_citations(self, user_message: str) -> list[dict[str, Any]]:
        """Build the auto-context citations for ``user_message`` from the knowledge
        base — one per top hit, matching the TS server's field names. ``url`` is set
        only when the source is an http(s) URL (omitted otherwise). Empty when no
        knowledge is wired (the eventual_response then omits the array entirely)."""
        if self._knowledge is None:
            return []
        citations: list[dict[str, Any]] = []
        for hit in self._knowledge.query(user_message, AUTO_CONTEXT_LIMIT):
            citation: dict[str, Any] = {
                "id": hit.source,
                "title": hit.source,
                "snippet": _truncate(hit.content, CITATION_SNIPPET_MAX_CHARS),
                "score": hit.score,
            }
            if hit.source.startswith("http://") or hit.source.startswith("https://"):
                citation["url"] = hit.source
            citations.append(citation)
        return citations


def _build_user_content(text: str, images: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Build an OpenAI multimodal user-message ``content`` list from the turn's text
    and image attachments: a leading ``text`` part, then one ``image_url`` part per
    image (``{url, detail?}`` → ``{"image_url": {"url": ..., "detail"?: ...}}``, the
    ``detail`` hint omitted when absent). Mirrors the Rust ``UserImage`` → engine
    ``ImageContent`` mapping.

    Fail-soft (per the ``images`` schema): an entry without a usable string ``url`` is
    skipped rather than rejecting the turn; a ``detail`` that is not a string is
    dropped. A leading text part is always present so the message is never image-only."""
    parts: list[dict[str, Any]] = [{"type": "text", "text": text}]
    for image in images:
        if not isinstance(image, dict):
            continue
        url = image.get("url")
        if not isinstance(url, str) or not url:
            continue
        image_url: dict[str, Any] = {"url": url}
        detail = image.get("detail")
        if isinstance(detail, str) and detail:
            image_url["detail"] = detail
        parts.append({"type": "image_url", "image_url": image_url})
    return parts


def _truncate(value: str, max_chars: int) -> str:
    """Cap ``value`` at ``max_chars`` (matches the TS server's ``truncate``)."""
    return value if len(value) <= max_chars else value[:max_chars]


def _record_turn_usage(turn_span: Any, prompt: int, completion: int, cost_usd: float) -> None:
    """Record the turn's token counts and cost — only the parts actually measured.

    Counts are omitted when the engine reported none: absent is honest, ``0`` is a lie
    (a grounded turn always consumes prompt tokens). Cost is judged separately, because
    the gateway reports it on an HTTP header while usage arrives on an SSE chunk — either
    can turn up without the other. A non-positive cost becomes an explicit ``unpriced``
    marker rather than a confident ``$0.00``."""
    if prompt or completion:
        turn_span.set_attribute(telemetry.GEN_AI_USAGE_INPUT_TOKENS, prompt)
        turn_span.set_attribute(telemetry.GEN_AI_USAGE_OUTPUT_TOKENS, completion)
    if cost_usd > 0 and math.isfinite(cost_usd):
        turn_span.set_attribute(telemetry.GEN_AI_USAGE_COST_USD, cost_usd)
    else:
        turn_span.set_attribute(telemetry.COST_UNAVAILABLE, telemetry.COST_UNAVAILABLE_UNPRICED)


def _emit_tool_span(event: ToolCallEvent, conversation_id: str, org_id: str | None) -> None:
    """Emit a ``gen_ai.tool`` child span for a tool call, carrying the tool name and
    the redacted JSON arguments (mirrors the Rust runner's per-tool child span). The
    span opens as a child of the current ``gen_ai.chat`` turn span and closes
    immediately — a marker, not a duration measured around execution (the engine owns
    tool execution). No-op cost until a tracer provider is installed.

    It repeats the turn's identifiers rather than relying on the parent: the OTLP ingest
    builds a span's attributes from the resource attrs plus THAT span's own, with no
    parent inheritance. Omitting ``gen_ai.system`` is worse than losing the join — the
    ingest's LLM-event gate keys on it, so bare tool spans are DISCARDED. Rust's were,
    for their entire existence."""
    attributes: dict[str, Any] = {
        telemetry.GEN_AI_SYSTEM: telemetry.SYSTEM_NAME,
        telemetry.GEN_AI_OPERATION_NAME: telemetry.OPERATION_TOOL,
        telemetry.GEN_AI_CONVERSATION_ID: conversation_id,
        telemetry.GEN_AI_TOOL_NAME: event.name,
        telemetry.GEN_AI_TOOL_ARGUMENTS: telemetry.redact_tool_arguments(event.arguments or ""),
    }
    if org_id:
        attributes[telemetry.SMOOAI_ORG_ID] = org_id
    with telemetry.tracer().start_as_current_span(
        telemetry.SPAN_TOOL,
        attributes=attributes,
    ):
        pass


def _tool_call_state(event: ToolCallEvent) -> dict[str, Any]:
    """The ``stream_chunk`` state for a requested tool call (matches the Rust/C#
    ``rawResponse.toolCall`` shape). ``event.arguments`` is a raw JSON string."""
    try:
        arguments: Any = json.loads(event.arguments) if event.arguments else {}
    except (json.JSONDecodeError, TypeError):
        arguments = event.arguments
    return {"rawResponse": {"toolCall": {"name": event.name, "arguments": arguments}}}


def _tool_call_state_from(name: str, arguments: Any) -> dict[str, Any]:
    """The ``stream_chunk`` toolCall state built from an already-parsed ``arguments``
    dict (the shape the engine's ``HumanApprovalRequest`` carries). Used to emit a
    gated tool's deferred toolCall chunk from the HumanGate."""
    return {"rawResponse": {"toolCall": {"name": name, "arguments": arguments}}}


def _tool_result_state(event: ToolResultEvent) -> dict[str, Any]:
    """The ``stream_chunk`` state for a tool result. The engine folds tool failures
    into the result string, so detect that to set ``isError`` (mirrors the C#
    ``ToolResultState`` convention)."""
    result_text = event.result or ""
    is_error = result_text.startswith("Error:") or result_text.startswith("Denied by human:")
    return {"rawResponse": {"toolResult": {"name": event.name, "isError": is_error, "result": result_text}}}
