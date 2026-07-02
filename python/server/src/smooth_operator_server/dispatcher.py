"""Routes an inbound protocol frame to the right handler and emits the response
event(s) to a sink.

The Python analog of the C# ``FrameDispatcher`` and the Rust ``handle_frame``.
Transport-agnostic: the WebSocket server calls :meth:`FrameDispatcher.dispatch`
per inbound frame and writes the sink's events back over the socket. One dispatcher
is bound to one connection's :class:`AccessContext` (resolved from the ``?token=``
slot) so retrieval for each turn is scoped to it.
"""

from __future__ import annotations

import asyncio
import json
import uuid
from typing import Any

from smooth_operator_core import Knowledge

from . import protocol
from .agent_config import AgentConfig
from .auth import AccessContext
from .confirmation import ConfirmationRegistry
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
        confirm_tools: list[str] | None = None,
        confirmations: ConfirmationRegistry | None = None,
        agent_configs: dict[str, AgentConfig] | None = None,
    ) -> None:
        self._store = store
        self._chat_client = chat_client
        self._knowledge = knowledge
        self._access = access if access is not None else AccessContext.ANONYMOUS  # type: ignore[attr-defined]
        self._system_prompt = system_prompt
        self._model = model
        self._tools = tools or []
        #: Tool-name patterns gated behind human confirmation (empty → HITL off).
        self._confirm_tools = confirm_tools or []
        #: Session-keyed pending-confirmation registry shared with the runner so a
        #: `confirm_tool_action` frame resolves the future a parked turn awaits.
        #: Created on demand (one per connection) when HITL is enabled.
        self._confirmations = confirmations if confirmations is not None else ConfirmationRegistry()
        #: Per-agent config keyed by agentId (SMOODEV-590). Resolved per turn from the
        #: session's agent; empty → every turn uses the server-wide default prompt.
        self._agent_configs = agent_configs or {}
        #: Spawned turn tasks kept alive (the event loop only holds weak refs to
        #: tasks); cleared as each completes so they don't accumulate.
        self._turn_tasks: set[asyncio.Task[Any]] = set()

    async def wait_for_turns(self) -> None:
        """Await every in-flight spawned ``send_message`` turn to completion.

        ``send_message`` runs its turn as a background task (so the read loop stays
        free to receive a `confirm_tool_action` while a turn is parked). The
        connection loop calls this in its teardown so an in-flight turn finishes —
        and its `eventual_response` is flushed — before the writer stops and the
        backplane detach runs (preserves the graceful-drain contract)."""
        if self._turn_tasks:
            await asyncio.gather(*tuple(self._turn_tasks), return_exceptions=True)

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
            elif action == "confirm_tool_action":
                self._handle_confirm_tool_action(frame, request_id, sink)
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
        #    Resolve the session's per-agent config (SMOODEV-590); None → the
        #    server-wide default prompt drives the turn (behavior unchanged).
        agent_config = self._agent_configs.get(session.agent_id)
        runner = TurnRunner(
            self._chat_client,
            self._store,
            knowledge=self._knowledge,
            system_prompt=self._system_prompt,
            model=self._model,
            tools=self._tools,
            confirm_tools=self._confirm_tools,
            confirmations=self._confirmations,
            agent_config=agent_config,
        )

        # Run the turn as a background task, NOT awaited inline. A turn that calls a
        # confirmation-gated tool **parks** awaiting a later `confirm_tool_action`
        # frame; the connection's read loop dispatches that frame, so awaiting the
        # turn here would block the reader and deadlock (the confirm could never be
        # read). Spawning frees the reader to receive the confirmation while the turn
        # streams its events through the sink. Mirrors the Rust `tokio::spawn`.
        # The 202 ack above is already on the wire (the reader pumps the writer),
        # and the terminal `eventual_response` is emitted from the task on completion.
        request_id_str: str = request_id
        session_id_str: str = session_id

        async def _run_turn() -> None:
            try:
                result = await runner.run(
                    session.conversation_id, request_id_str, message, sink, session_id=session_id_str
                )
            except Exception:
                # Mirror the dispatcher's outer guard: a turn failure surfaces a clean
                # error and keeps the connection alive (detail stays server-side).
                sink(protocol.error(request_id_str, "INTERNAL_ERROR", "Internal error processing the request."))
                return
            sink(
                protocol.eventual_response(
                    request_id_str,
                    200,
                    result.message_id,
                    protocol.general_agent_response(result.reply),
                    needs_escalation=False,
                    citations=result.citations or None,
                )
            )

        task = asyncio.ensure_future(_run_turn())
        self._turn_tasks.add(task)
        task.add_done_callback(self._turn_tasks.discard)

    def _handle_confirm_tool_action(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        """``confirm_tool_action`` — resume a turn parked on a write-tool confirmation.

        Per ``spec/actions/confirm-tool-action.schema.json`` the client replies with
        ``{action, sessionId, requestId, approved}`` to a ``write_confirmation_required``
        event. We resolve the session's pending confirmation with the verdict: the
        parked ``HumanGate`` returns and the turn resumes (runs the tool on approve,
        skips it with a rejection result on deny). There is no dedicated response
        event — continuation is signalled by the resumed streaming sequence; we ack
        with an ``immediate_response``. Resolving takes the future out, so a duplicate
        confirm is a clean ``NO_PENDING_CONFIRMATION`` no-op. Fails closed: a missing
        ``sessionId`` or non-bool ``approved`` is rejected (never silently approve)."""
        session_id = frame.get("sessionId")
        if not session_id:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "confirm_tool_action requires a 'sessionId'"))
            return

        approved = frame.get("approved")
        if not isinstance(approved, bool):
            sink(protocol.error(request_id, "VALIDATION_ERROR", "confirm_tool_action requires a boolean 'approved'"))
            return

        if not self._confirmations.resolve(session_id, approved):
            sink(
                protocol.error(
                    request_id,
                    "NO_PENDING_CONFIRMATION",
                    f"no tool action is awaiting confirmation for session '{session_id}'",
                )
            )
            return

        sink(
            protocol.immediate_response(
                request_id,
                200,
                "Tool action approved" if approved else "Tool action rejected",
                {"sessionId": session_id, "approved": approved},
            )
        )
