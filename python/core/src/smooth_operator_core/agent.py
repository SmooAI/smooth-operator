"""The Python smooth-operator core: a native agentic loop.

Phase-0 sibling of the C# ``SmoothAgent`` (``dotnet/core``) and the Rust
reference engine. Drives an agentic tool-calling loop over any OpenAI-compatible
chat client (the ``openai`` SDK pointed at a gateway): inject retrieved
knowledge, call the model, run any requested tools, feed results back, and loop
until the model answers without a tool call or the iteration budget is hit.

Deliberately minimal (no compaction / budget / checkpointing yet) — those layer
on exactly as they did when the C# core grew past Phase 0. The point of Phase 0
is a real, in-process engine that passes the shared eval suite against the live
gateway.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, Protocol

from .knowledge import InMemoryKnowledge


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
    knowledge: InMemoryKnowledge | None = None
    knowledge_top_k: int = 4
    tools: list[Tool] = field(default_factory=list)


@dataclass
class AgentRunResponse:
    """The result of a turn: the final assistant text plus a little provenance."""

    text: str
    iterations: int
    tool_calls: int


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
        kb = self._options.knowledge
        if kb is not None:
            hits = kb.query(message, self._options.knowledge_top_k)
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

    async def run(self, message: str, history: list[dict[str, Any]] | None = None) -> AgentRunResponse:
        """Run a single turn. ``history`` is prior OpenAI-format messages (multi-turn)."""
        messages: list[dict[str, Any]] = []
        system = self._build_system(message)
        if system:
            messages.append({"role": "system", "content": system})
        if history:
            messages.extend(history)
        messages.append({"role": "user", "content": message})

        tool_specs = self._tool_specs()
        tool_call_count = 0
        last_text = ""

        for iteration in range(1, self._options.max_iterations + 1):
            response = await self._client.chat.completions.create(
                model=self._options.model,
                messages=messages,
                tools=tool_specs,
                temperature=self._options.temperature,
                max_tokens=self._options.max_tokens,
            )
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

            if not choice.tool_calls:
                return AgentRunResponse(text=last_text, iterations=iteration, tool_calls=tool_call_count)

            for tc in choice.tool_calls:
                tool_call_count += 1
                result = await self._dispatch_tool(tc.function.name, tc.function.arguments)
                messages.append({"role": "tool", "tool_call_id": tc.id, "content": result})

        return AgentRunResponse(text=last_text, iterations=self._options.max_iterations, tool_calls=tool_call_count)

    async def _dispatch_tool(self, name: str, raw_arguments: str) -> str:
        import json

        tool = self._tools_by_name.get(name)
        if tool is None:
            return f"error: unknown tool '{name}'"
        try:
            args = json.loads(raw_arguments) if raw_arguments else {}
        except json.JSONDecodeError:
            return f"error: tool '{name}' received invalid JSON arguments"
        try:
            return await tool.execute(args)
        except Exception as exc:  # noqa: BLE001 — surface tool failures to the model, don't crash the turn
            return f"error: tool '{name}' failed: {exc}"
