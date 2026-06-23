"""The Python smooth-operator core: a native agentic loop.

Phase-0 sibling of the C# ``SmoothAgent`` (``dotnet/core``) and the Rust
reference engine. Drives an agentic tool-calling loop over any OpenAI-compatible
chat client (the ``openai`` SDK pointed at a gateway): inject retrieved
knowledge, call the model, run any requested tools, feed results back, and loop
until the model answers without a tool call or the iteration budget is hit.

Phase 1 adds context compaction and token/cost budgeting; further features
(checkpointing, rerank, memory, sub-agents, vector knowledge) layer on as they did
when the C# core grew past Phase 0.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, Protocol

from .cast import Clearance
from .checkpoint import Checkpoint, CheckpointStore
from .compaction import compact
from .cost import CostBudget, CostTracker, ModelPricing, Usage
from .human_gate import HumanApprovalRequest, HumanGate
from .knowledge import Knowledge
from .memory import Memory
from .rerank import NoopReranker, Reranker
from .thread import SmoothAgentThread


class Tool(Protocol):
    """A callable tool the agent may invoke. Mirrors the reference engines' tool seam."""

    name: str
    description: str
    parameters: dict[str, Any]

    async def execute(self, arguments: dict[str, Any]) -> str: ...


@dataclass
class FunctionTool:
    """Wrap an ordinary async function as a :class:`Tool` (akin to AIFunctionFactory)."""

    name: str
    description: str
    parameters: dict[str, Any]
    func: Callable[[dict[str, Any]], Awaitable[str]]

    async def execute(self, arguments: dict[str, Any]) -> str:
        return await self.func(arguments)


@dataclass
class AgentOptions:
    """Configuration for a :class:`SmoothAgent` turn. Mirrors the C# ``AgentOptions``."""

    instructions: str = ""
    model: str = "claude-haiku-4-5"
    max_iterations: int = 8
    max_tokens: int = 512
    temperature: float = 0.0
    knowledge: Knowledge | None = None
    knowledge_top_k: int = 4
    #: Reranker applied to retrieved hits before injection (defaults to passthrough).
    reranker: Reranker = field(default_factory=NoopReranker)
    #: Candidate pool size to retrieve before reranking. When greater than
    #: ``knowledge_top_k``, more documents are fetched, reranked, and trimmed to
    #: ``knowledge_top_k`` — so the reranker can promote a better candidate.
    knowledge_candidate_k: int = 0
    #: Optional long-term memory; relevant entries are recalled into context each turn.
    memory: Memory | None = None
    #: How many memory entries to recall per turn.
    memory_top_k: int = 4
    tools: list[Tool] = field(default_factory=list)
    #: Approximate token budget for the context window. Before each model call,
    #: older non-system messages are dropped (sliding window) to stay under it.
    #: ``0`` disables compaction.
    max_context_tokens: int = 8000
    #: Optional ceiling for the turn (token and/or USD). The turn stops early once
    #: a model call pushes accumulated usage/cost over the budget.
    budget: CostBudget | None = None
    #: Per-model pricing override for cost accounting (defaults to DEFAULT_PRICING).
    pricing: dict[str, ModelPricing] | None = None
    #: Optional store for persisting/resuming the conversation. When set together
    #: with ``conversation_id``, prior messages are loaded at the start of a turn
    #: and the updated conversation is saved at the end.
    checkpoint_store: CheckpointStore | None = None
    #: Conversation id for the checkpoint store (required to use checkpointing).
    conversation_id: str | None = None
    #: Optional tool-access policy. When set, a tool the clearance forbids is not
    #: dispatched — a "tool not permitted" result is returned to the model instead.
    #: ``None`` allows every tool (the prior behaviour).
    clearance: Clearance | None = None
    #: Optional human-in-the-loop gate. When set, the agent asks it for approval
    #: before running any tool call for which ``requires_approval`` returns true.
    #: A denied call is not executed; the model is told it was denied and can adapt.
    human_gate: HumanGate | None = None
    #: Which tool calls need human approval (e.g. writes / destructive actions),
    #: given the tool name and parsed arguments. Default: none. Only consulted when
    #: ``human_gate`` is set. Example::
    #:
    #:     lambda name, args: name in {"delete_record", "send_email"}
    requires_approval: Callable[[str, dict[str, Any]], bool] | None = None


@dataclass
class AgentRunResponse:
    """The result of a turn: the final assistant text plus a little provenance."""

    text: str
    iterations: int
    tool_calls: int
    usage: Usage = field(default_factory=Usage)
    cost_usd: float = 0.0
    #: True if the turn stopped because the cost/token budget was hit.
    budget_exceeded: bool = False


def _extract_usage(response: Any) -> Usage:
    """Pull token usage from an OpenAI-shaped response, defaulting to zero when
    absent (e.g. a fake client in tests)."""
    u = getattr(response, "usage", None)
    if u is None:
        return Usage()
    return Usage(
        prompt_tokens=int(getattr(u, "prompt_tokens", 0) or 0),
        completion_tokens=int(getattr(u, "completion_tokens", 0) or 0),
    )


class SmoothAgent:
    """A native, in-process agent. Construct with an OpenAI-compatible async client
    (e.g. ``openai.AsyncOpenAI(base_url=..., api_key=...)``) and :class:`AgentOptions`.
    """

    def __init__(self, chat_client: Any, options: AgentOptions) -> None:
        if chat_client is None:
            raise ValueError("chat_client is required")
        self._client = chat_client
        self._options = options
        self._tools_by_name = {t.name: t for t in options.tools}

    def _build_system(self, message: str) -> str:
        system = self._options.instructions or ""

        mem = self._options.memory
        if mem is not None:
            recalled = mem.recall(message, self._options.memory_top_k)
            if recalled:
                block = "\n".join(f"- {e.text}" for e in recalled)
                system = (
                    system + "\n\nRelevant memory (things you remember about this user/context):\n" + block
                ).strip()

        kb = self._options.knowledge
        if kb is not None:
            top_k = self._options.knowledge_top_k
            candidate_k = max(self._options.knowledge_candidate_k, top_k)
            hits = kb.query(message, candidate_k)
            hits = self._options.reranker.rerank(message, hits)[:top_k]
            if hits:
                block = "\n\n".join(f"[{h.source}] {h.content}" for h in hits)
                system = (
                    system
                    + "\n\nKnowledge base (ground all facts ONLY in this; if it is not here, say you don't know):\n"
                    + block
                ).strip()
        return system

    def _tool_specs(self) -> list[dict[str, Any]] | None:
        if not self._options.tools:
            return None
        return [
            {
                "type": "function",
                "function": {"name": t.name, "description": t.description, "parameters": t.parameters},
            }
            for t in self._options.tools
        ]

    async def run(
        self,
        message: str,
        history: list[dict[str, Any]] | None = None,
        thread: SmoothAgentThread | None = None,
    ) -> AgentRunResponse:
        """Run a single turn.

        ``history`` is prior OpenAI-format messages (multi-turn). ``thread``, when
        given, is a :class:`SmoothAgentThread` carrying the conversation across runs:
        the turn is seeded from the thread's messages, and this turn's new user +
        assistant (+ tool) messages are appended back to it before returning. The
        thread takes precedence over ``history`` as the prior context.
        """
        messages: list[dict[str, Any]] = []
        system = self._build_system(message)
        if system:
            messages.append({"role": "system", "content": system})

        # Source prior conversation: the thread (if passed) wins, then the checkpoint
        # store (if configured), then the explicit ``history`` argument.
        cp_store = self._options.checkpoint_store
        cp_id = self._options.conversation_id
        prior = history
        if cp_store is not None and cp_id is not None:
            loaded = cp_store.load(cp_id)
            if loaded is not None:
                prior = loaded.messages
        if thread is not None:
            prior = list(thread.messages)
        if prior:
            messages.extend(prior)
        user_msg = {"role": "user", "content": message}
        messages.append(user_msg)

        # Track this turn's new messages by identity so they can be appended back to
        # the thread on exit. Index-based slicing would be unsafe — compaction may
        # drop/reorder ``messages`` mid-turn.
        turn_messages: list[dict[str, Any]] = [user_msg]

        tool_specs = self._tool_specs()
        tool_call_count = 0
        last_text = ""
        tracker = CostTracker()

        try:
            for iteration in range(1, self._options.max_iterations + 1):
                # Keep the context window within budget before each model call.
                messages = compact(messages, self._options.max_context_tokens)
                response = await self._client.chat.completions.create(
                    model=self._options.model,
                    messages=messages,
                    tools=tool_specs,
                    temperature=self._options.temperature,
                    max_tokens=self._options.max_tokens,
                )
                tracker.record(self._options.model, _extract_usage(response), self._options.pricing)
                choice = response.choices[0].message
                last_text = choice.content or ""

                # Append the assistant turn (OpenAI wire shape) so tool results pair to it.
                assistant_msg: dict[str, Any] = {"role": "assistant", "content": choice.content or ""}
                if choice.tool_calls:
                    assistant_msg["tool_calls"] = [
                        {
                            "id": tc.id,
                            "type": "function",
                            "function": {"name": tc.function.name, "arguments": tc.function.arguments},
                        }
                        for tc in choice.tool_calls
                    ]
                messages.append(assistant_msg)
                turn_messages.append(assistant_msg)

                # Stop early if this turn has hit its token/cost budget.
                if tracker.exceeds(self._options.budget):
                    return AgentRunResponse(
                        text=last_text,
                        iterations=iteration,
                        tool_calls=tool_call_count,
                        usage=tracker.usage,
                        cost_usd=tracker.cost_usd,
                        budget_exceeded=True,
                    )

                if not choice.tool_calls:
                    return AgentRunResponse(
                        text=last_text,
                        iterations=iteration,
                        tool_calls=tool_call_count,
                        usage=tracker.usage,
                        cost_usd=tracker.cost_usd,
                    )

                for tc in choice.tool_calls:
                    tool_call_count += 1
                    result = await self._dispatch_tool(tc.function.name, tc.function.arguments)
                    tool_msg = {"role": "tool", "tool_call_id": tc.id, "content": result}
                    messages.append(tool_msg)
                    turn_messages.append(tool_msg)

            return AgentRunResponse(
                text=last_text,
                iterations=self._options.max_iterations,
                tool_calls=tool_call_count,
                usage=tracker.usage,
                cost_usd=tracker.cost_usd,
            )
        finally:
            # Persist the conversation (sans system prompt, which is rebuilt each turn).
            if cp_store is not None and cp_id is not None:
                cp_store.save(
                    Checkpoint(conversation_id=cp_id, messages=[m for m in messages if m.get("role") != "system"])
                )
            # Append this turn's new messages (user + assistant + tool, never system)
            # back to the thread so the next run sees the full conversation.
            if thread is not None:
                thread.extend(turn_messages)

    async def _dispatch_tool(self, name: str, raw_arguments: str) -> str:
        import json

        # Enforce the role's tool clearance before dispatch: a forbidden tool is
        # never executed — the model is told it isn't permitted, mirroring how the
        # loop surfaces other tool errors.
        clearance = self._options.clearance
        if clearance is not None and not clearance.is_allowed(name):
            return f"error: tool '{name}' is not permitted for this role"

        tool = self._tools_by_name.get(name)
        if tool is None:
            return f"error: unknown tool '{name}'"
        try:
            args = json.loads(raw_arguments) if raw_arguments else {}
        except json.JSONDecodeError:
            return f"error: tool '{name}' received invalid JSON arguments"

        # Human-in-the-loop: pause for approval before running a flagged (write/sensitive)
        # tool. A denial is fed back to the model as a result — the tool never runs.
        gate = self._options.human_gate
        needs_approval = self._options.requires_approval
        if gate is not None and needs_approval is not None and needs_approval(name, args):
            request = HumanApprovalRequest(tool_name=name, arguments=args, prompt=f"Approve calling tool '{name}'?")
            decision = await gate.request_approval(request)
            if not decision.is_approved:
                return f"Denied by human: {decision.reason or 'no reason given'}"

        try:
            return await tool.execute(args)
        except Exception as exc:  # noqa: BLE001 — surface tool failures to the model, don't crash the turn
            return f"error: tool '{name}' failed: {exc}"


def delegate_tool(name: str, description: str, child: SmoothAgent, task_property: str = "task") -> FunctionTool:
    """Build a :class:`Tool` that delegates a subtask to a child :class:`SmoothAgent`.

    A sub-agent is just a tool backed by another agent: the model calls this tool
    with a ``task`` argument, the child agent runs that task, and the child's final
    reply becomes the tool result — composing with the existing tool loop, no
    special wiring. The child can have its own instructions, tools, knowledge, etc.
    """

    async def _run(args: dict[str, Any]) -> str:
        task = str(args.get(task_property, ""))
        result = await child.run(task)
        return result.text

    return FunctionTool(
        name=name,
        description=description,
        parameters={
            "type": "object",
            "properties": {
                task_property: {"type": "string", "description": "The subtask for the sub-agent to perform."}
            },
            "required": [task_property],
        },
        func=_run,
    )
