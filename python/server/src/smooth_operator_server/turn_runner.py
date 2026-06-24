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
    Knowledge,
    SmoothAgent,
    SmoothAgentThread,
    TextEvent,
    ToolCallEvent,
    ToolResultEvent,
)

from . import protocol
from .session_store import MessageDirection, SessionStore

#: Max prior turns replayed into the thread for memory (bounds context growth).
MAX_PRIOR_MESSAGES = 50

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
    ) -> None:
        self._chat_client = chat_client
        self._store = store
        self._knowledge = knowledge
        self._system_prompt = system_prompt or DEFAULT_SYSTEM_PROMPT
        self._model = model
        self._tools = tools or []

    async def run(
        self,
        conversation_id: str,
        request_id: str,
        user_message: str,
        sink: Sink,
    ) -> TurnResult:
        # 1. Build the agent. The knowledge base (when present) auto-injects the
        #    top hits into the system prompt — the engine handles retrieval + rerank
        #    internally, mirroring the C# `new SmoothAgent(..., Knowledge = ...)`.
        options_kwargs: dict[str, Any] = {
            "instructions": self._system_prompt,
            "knowledge": self._knowledge,
        }
        if self._tools:
            options_kwargs["tools"] = self._tools
        if self._model is not None:
            options_kwargs["model"] = self._model
        agent = SmoothAgent(self._chat_client, AgentOptions(**options_kwargs))

        # 2. Replay prior history as the thread (before persisting this turn's
        #    inbound), so the model sees turn 1 when answering turn 2. Mirrors the
        #    Rust `load_prior_messages` + the C# thread replay.
        thread = SmoothAgentThread(id=conversation_id)
        for message in await self._store.list_messages(conversation_id, MAX_PRIOR_MESSAGES):
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
        async for event in agent.run_stream(user_message, thread=thread):
            if isinstance(event, TextEvent):
                if event.text:
                    reply_parts.append(event.text)
                    sink(protocol.stream_token(request_id, event.text))
            elif isinstance(event, ToolCallEvent):
                sink(protocol.stream_chunk(request_id, event.name, _tool_call_state(event)))
            elif isinstance(event, ToolResultEvent):
                sink(protocol.stream_chunk(request_id, event.name, _tool_result_state(event)))
            elif isinstance(event, DoneEvent):
                final_text = event.response.text

        # The DoneEvent's text wins (it's the engine's authoritative final), falling
        # back to the concatenated streamed deltas if it's empty.
        reply = final_text if final_text else "".join(reply_parts)

        # 5. Persist the outbound reply and return.
        outbound = await self._store.append_message(conversation_id, MessageDirection.OUTBOUND, reply)
        return TurnResult(reply=reply, message_id=outbound.id, citations=[])


def _tool_call_state(event: ToolCallEvent) -> dict[str, Any]:
    """The ``stream_chunk`` state for a requested tool call (matches the Rust/C#
    ``rawResponse.toolCall`` shape)."""
    try:
        arguments: Any = json.loads(event.arguments) if event.arguments else {}
    except (json.JSONDecodeError, TypeError):
        arguments = event.arguments
    return {"rawResponse": {"toolCall": {"name": event.name, "arguments": arguments}}}


def _tool_result_state(event: ToolResultEvent) -> dict[str, Any]:
    """The ``stream_chunk`` state for a tool result. The engine folds tool failures
    into the result string, so detect that to set ``isError`` (mirrors the C#
    ``ToolResultState`` convention)."""
    result_text = event.result or ""
    is_error = result_text.startswith("Error:") or result_text.startswith("Denied by human:")
    return {"rawResponse": {"toolResult": {"name": event.name, "isError": is_error, "result": result_text}}}
