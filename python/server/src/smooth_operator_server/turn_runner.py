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

import json
from dataclasses import dataclass, field
from typing import Any, Callable

from smooth_operator_core import (
    AgentOptions,
    DoneEvent,
    HumanApprovalRequest,
    HumanApprovalResponse,
    Knowledge,
    SmoothAgent,
    SmoothAgentThread,
    TextEvent,
    ToolCallEvent,
    ToolResultEvent,
)

from . import protocol
from .agent_config import AgentConfig
from .confirmation import ConfirmationRegistry
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

#: A sink the runner emits ready-to-send protocol event dicts through (the
#: connection forwards them over the socket). Sync send, like the Rust ``sink``.
Sink = Callable[[dict[str, Any]], None]


@dataclass(frozen=True)
class TurnResult:
    """What a completed turn produced (the analog of the Rust/C# ``TurnResult``)."""

    reply: str
    message_id: str
    citations: list[dict[str, Any]] = field(default_factory=list)


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
    ) -> None:
        self._chat_client = chat_client
        self._store = store
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

    def _is_gated(self, tool_name: str) -> bool:
        """True when ``tool_name`` matches a confirmation-gated pattern (substring,
        like the Rust hook). Only meaningful when a confirmation registry is wired."""
        if self._confirmations is None:
            return False
        return any(pattern in tool_name for pattern in self._confirm_tools)

    async def run(
        self,
        conversation_id: str,
        request_id: str,
        user_message: str,
        sink: Sink,
        session_id: str | None = None,
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

        # 1. Build the agent. The knowledge base (when present) auto-injects the
        #    top hits into the system prompt — the engine handles retrieval + rerank
        #    internally, mirroring the C# `new SmoothAgent(..., Knowledge = ...)`.
        options_kwargs: dict[str, Any] = {
            "instructions": self._assemble_system_prompt(current_step_id, is_first_turn=not prior_messages),
            "knowledge": self._knowledge,
        }
        if self._tools:
            options_kwargs["tools"] = self._tools
        if self._model is not None:
            options_kwargs["model"] = self._model

        # Write-confirmation HITL: when configured with tool patterns AND a registry
        # is present, install a HumanGate that parks the turn before a gated tool
        # runs (emit `write_confirmation_required`, await the client's verdict via
        # the session-keyed registry). With no patterns (the default) no gate is
        # installed → no tool ever parks → behavior identical to before HITL. The
        # gate keys its pending future by `session_id`, so a `confirm_tool_action`
        # frame (also keyed by sessionId) routes back here.
        confirm_session = session_id or conversation_id
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

        # 3. Persist the inbound user message.
        await self._store.append_message(conversation_id, MessageDirection.INBOUND, user_message)

        # 4. Stream the turn: a stream_token per text delta, a stream_chunk per tool
        #    call / tool result (mirrors the Rust runner translating the engine's
        #    AgentEvent stream). The terminal DoneEvent carries the final
        #    AgentRunResponse, whose `text` is authoritative for the reply.
        reply_parts: list[str] = []
        final_text: str | None = None
        try:
            async for event in agent.run_stream(user_message, thread=thread):
                if isinstance(event, TextEvent):
                    if event.text:
                        reply_parts.append(event.text)
                        sink(protocol.stream_token(request_id, event.text))
                elif isinstance(event, ToolCallEvent):
                    # DEFER a confirmation-gated tool's toolCall chunk: it is emitted
                    # from the gate AFTER `write_confirmation_required`, so the wire
                    # order matches the reference (Rust) server. Non-gated tools emit
                    # their chunk inline as before.
                    if self._is_gated(event.name):
                        continue
                    sink(protocol.stream_chunk(request_id, event.name, _tool_call_state(event)))
                elif isinstance(event, ToolResultEvent):
                    sink(protocol.stream_chunk(request_id, event.name, _tool_result_state(event)))
                elif isinstance(event, DoneEvent):
                    final_text = event.response.text
        finally:
            # Turn over: drop any lingering pending confirmation so a stale entry
            # can't mis-route a later `confirm_tool_action` (mirrors the Rust
            # `(cfg.clear)(session_id)` at turn end). No-op when HITL is off.
            if self._confirmations is not None:
                self._confirmations.clear(confirm_session)

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

        return TurnResult(reply=reply, message_id=outbound.id, citations=citations)

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


def _truncate(value: str, max_chars: int) -> str:
    """Cap ``value`` at ``max_chars`` (matches the TS server's ``truncate``)."""
    return value if len(value) <= max_chars else value[:max_chars]


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
