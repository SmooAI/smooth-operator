"""Routes an inbound protocol frame to the right handler and emits the response
event(s) to a sink.

The Python analog of the C# ``FrameDispatcher`` and the Rust ``handle_frame``.
Transport-agnostic: the WebSocket server calls :meth:`FrameDispatcher.dispatch`
per inbound frame and writes the sink's events back over the socket. One dispatcher
is bound to one connection's :class:`AccessContext` (resolved from the ``?token=``
slot) so retrieval for each turn is scoped to it.
"""

from __future__ import annotations

import json
import uuid
from typing import Any

from smooth_operator_core import Knowledge

from . import protocol
from .auth import AccessContext
from .session_store import SessionStore
from .turn_runner import Sink, TurnRunner


class FrameDispatcher:
    """Dispatches one inbound frame by its ``action`` discriminator."""

    def __init__(
        self,
        store: SessionStore,
        chat_client: Any,
        knowledge: Knowledge | None = None,
        access: AccessContext | None = None,
        system_prompt: str | None = None,
        model: str | None = None,
        tools: list[Any] | None = None,
    ) -> None:
        self._store = store
        self._chat_client = chat_client
        self._knowledge = knowledge
        self._access = access if access is not None else AccessContext.ANONYMOUS  # type: ignore[attr-defined]
        self._system_prompt = system_prompt
        self._model = model
        self._tools = tools or []

    async def dispatch(self, raw_frame: str, sink: Sink) -> None:
        try:
            frame = json.loads(raw_frame)
        except (json.JSONDecodeError, TypeError) as exc:
            sink(protocol.error(None, "VALIDATION_ERROR", f"invalid JSON frame: {exc}"))
            return

        if not isinstance(frame, dict):
            sink(protocol.error(None, "VALIDATION_ERROR", "empty or non-object frame"))
            return

        action = frame.get("action")
        request_id = frame.get("requestId")

        try:
            if action == "ping":
                sink(protocol.pong(request_id))
            elif action == "create_conversation_session":
                await self._handle_create_session(frame, request_id, sink)
            elif action == "get_session":
                await self._handle_get_session(frame, request_id, sink)
            elif action == "send_message":
                await self._handle_send_message(frame, request_id, sink)
            elif action is None:
                sink(protocol.error(request_id, "VALIDATION_ERROR", "missing 'action' field"))
            else:
                sink(protocol.error(request_id, "UNSUPPORTED_ACTION", f"action '{action}' is not supported"))
        except Exception:
            # A handler failed mid-turn. Emit a clean error and KEEP the connection
            # alive — never drop the socket with no signal (exception detail stays
            # server-side, not leaked over the wire). Mirrors the C#/Rust handler.
            sink(protocol.error(request_id, "INTERNAL_ERROR", "Internal error processing the request."))

    async def _handle_create_session(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        session = await self._store.create_session(
            frame.get("agentId") or "",
            frame.get("userName"),
            frame.get("userEmail"),
        )
        data = {
            "sessionId": session.session_id,
            "conversationId": session.conversation_id,
            "agentId": session.agent_id,
            "agentName": session.agent_name,
            "userParticipantId": session.user_participant_id,
            "agentParticipantId": session.agent_participant_id,
        }
        sink(protocol.immediate_response(request_id, 200, "Session created", data))

    async def _handle_get_session(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        session_id = frame.get("sessionId")
        if not session_id:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "missing 'sessionId'"))
            return
        session = await self._store.get_session(session_id)
        if session is None:
            sink(protocol.error(request_id, "SESSION_NOT_FOUND", f"session '{session_id}' not found"))
            return
        data = {
            "sessionId": session.session_id,
            "conversationId": session.conversation_id,
            "agentId": session.agent_id,
            "agentName": session.agent_name,
            "userParticipantId": session.user_participant_id,
            "agentParticipantId": session.agent_participant_id,
        }
        sink(protocol.immediate_response(request_id, 200, "Session", data))

    async def _handle_send_message(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        # requestId is load-bearing for streaming correlation; generate one if the
        # client omitted it (mirrors the C# `requestId ??= Guid`).
        if not request_id:
            request_id = str(uuid.uuid4())

        session_id = frame.get("sessionId")
        if not session_id:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "missing 'sessionId'"))
            return

        session = await self._store.get_session(session_id)
        if session is None:
            sink(protocol.error(request_id, "SESSION_NOT_FOUND", f"session '{session_id}' not found"))
            return

        message = frame.get("message")
        if not isinstance(message, str) or not message.strip():
            sink(protocol.error(request_id, "VALIDATION_ERROR", "missing or empty 'message'"))
            return

        # No chat client → can't run an LLM turn. Return a clean error; the server
        # stays usable for protocol-only checks (mirrors the Rust LLM_UNAVAILABLE).
        if self._chat_client is None:
            sink(
                protocol.error(
                    request_id,
                    "LLM_UNAVAILABLE",
                    "no LLM gateway is configured; this server cannot serve send_message turns.",
                )
            )
            return

        # 1. Immediate ack (202).
        sink(protocol.immediate_response(request_id, 202, "Processing your request...", {}))

        # 2. Stream the turn through a runner scoped to this connection's access.
        runner = TurnRunner(
            self._chat_client,
            self._store,
            knowledge=self._knowledge,
            system_prompt=self._system_prompt,
            model=self._model,
            tools=self._tools,
        )
        result = await runner.run(session.conversation_id, request_id, message, sink)

        # 3. Terminal eventual_response.
        sink(
            protocol.eventual_response(
                request_id,
                200,
                result.message_id,
                protocol.general_agent_response(result.reply),
                needs_escalation=False,
                citations=result.citations or None,
            )
        )
