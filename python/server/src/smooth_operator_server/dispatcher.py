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
from .agent_config import (
    AgentConfigResolver,
    NoSessionAuthenticator,
    OtpRefusal,
    SessionAuthenticator,
    StaticAgentConfigResolver,
    filter_tools,
    gate_tools,
)
from .auth import AccessContext
from .confirmation import ConfirmationRegistry
from .otp import OtpContact, OtpInvalid, OtpService, OtpVerified
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
        agent_config_resolver: AgentConfigResolver | None = None,
        session_authenticator: SessionAuthenticator | None = None,
        judge_model: str | None = None,
        otp_service: OtpService | None = None,
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
        #: Per-agent config resolver (SMOODEV-590). Resolved per turn from the session's
        #: agent; the default (empty static resolver) returns None → the server-wide
        #: default prompt drives every turn.
        self._agent_config_resolver = agent_config_resolver or StaticAgentConfigResolver()
        #: Identity-verification seam gating end_user auth-level tools (fail-closed).
        self._session_authenticator = session_authenticator or NoSessionAuthenticator()
        #: End-user OTP identity-verification seam. When set, a turn whose gate refuses
        #: an `end_user` tool on an unverified session with a known contact triggers the
        #: OTP-offer flow, and `verify_otp` marks the session authenticated. `None` (the
        #: default) keeps the fail-closed behavior — refuse, no OTP offered.
        self._otp_service = otp_service
        #: Fast model for the post-turn workflow judge (None → runner's default).
        self._judge_model = judge_model
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
            elif action == "list_conversations":
                await self._handle_list_conversations(frame, request_id, sink)
            elif action == "get_session":
                await self._handle_get_session(frame, request_id, sink)
            elif action == "send_message":
                await self._handle_send_message(frame, request_id, sink)
            elif action == "confirm_tool_action":
                self._handle_confirm_tool_action(frame, request_id, sink)
            elif action == "verify_otp":
                await self._handle_verify_otp(frame, request_id, sink)
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
        # Resume: a `conversationId` naming an existing conversation binds the new session
        # to it (reuses id + history); absent/unknown → a fresh conversation. The response
        # echoes conversationId either way, so a resuming client sees the id it passed.
        # Mirrors the Rust/Go/TS reference. th-d5b446.
        raw_conv_id = frame.get("conversationId")
        conversation_id = raw_conv_id if isinstance(raw_conv_id, str) and raw_conv_id else None
        session = await self._store.create_session(
            frame.get("agentId") or "",
            frame.get("userName"),
            frame.get("userEmail"),
            conversation_id,
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

    async def _handle_list_conversations(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        """``list_conversations`` — the conversation-sidebar / resume substrate. Returns
        the store's conversations that have at least one message (empties, minted every
        page-load, are dropped), most-recent-first, each ``{conversationId, title,
        updatedAt, messageCount}`` where ``title`` is a first-inbound preview. Optional
        input: ``limit`` (default 50). A client resumes one by passing its
        ``conversationId`` to ``create_conversation_session``. Mirrors the Rust/Go/TS
        reference. th-d5b446."""
        raw_limit = frame.get("limit")
        limit = (
            raw_limit
            if isinstance(raw_limit, int) and not isinstance(raw_limit, bool) and raw_limit > 0
            else _DEFAULT_LIST_LIMIT
        )

        summaries = await self._store.list_conversations()
        # Most-recent-first (empties already dropped by the store), then cap.
        summaries.sort(key=lambda c: c.updated_at, reverse=True)
        conversations = [
            {
                "conversationId": c.conversation_id,
                "title": _conversation_title(c.first_inbound_text, f"Conversation {c.conversation_id}"),
                "updatedAt": c.updated_at.isoformat(),
                "messageCount": c.message_count,
            }
            for c in summaries[:limit]
        ]
        sink(protocol.immediate_response(request_id, 200, "Conversations", {"conversations": conversations}))

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
        agent_config = await self._agent_config_resolver.resolve(session.agent_id)
        # Resolve the turn's identity bit once: the session's OTP-verified state (set
        # by a prior successful verify_otp) OR the host SessionAuthenticator seam. This
        # is the Python analog of threading the Rust session's `otpVerified` bit into
        # `build_auth_gate` (replacing the hardcoded false).
        session_authed = await self._store.is_session_authenticated(
            session.session_id
        ) or await self._session_authenticator.is_authenticated(session.conversation_id)
        # A shared slot the gated tools record an `end_user` refusal into, so after the
        # turn we can decide whether to offer OTP (installed service + known contact).
        otp_refusal = OtpRefusal()
        # Restrict the tool set to the agent's allow-list (empty/None → full set), then
        # wrap survivors to enforce per-tool authLevel + deliver per-tool config.
        agent_tools = filter_tools(self._tools, agent_config)
        agent_tools = gate_tools(agent_tools, agent_config, session_authed, otp_refusal)
        runner = TurnRunner(
            self._chat_client,
            self._store,
            knowledge=self._knowledge,
            system_prompt=self._system_prompt,
            model=self._model,
            tools=agent_tools,
            confirm_tools=self._confirm_tools,
            confirmations=self._confirmations,
            agent_config=agent_config,
            judge_model=self._judge_model,
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
            # If the gate refused an `end_user` tool this turn for lack of a verified
            # session, and a host OTP service is installed and the session has a contact
            # to reach, offer the OTP flow (prompt → dispatch → ack) BEFORE the terminal
            # response — mirrors the Rust `offer_otp`. The reference server does not
            # park/auto-resume; the client verifies via `verify_otp` and re-sends.
            await self._maybe_offer_otp(otp_refusal, session, request_id_str, sink)
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

    async def _maybe_offer_otp(self, refusal: OtpRefusal, session: Any, request_id: str, sink: Sink) -> None:
        """Emit the OTP-offer sequence for a turn whose ``end_user`` tool was refused
        for lack of a verified session: ``otp_verification_required`` (prompt the
        client), then :meth:`OtpService.send_otp`, then ``otp_sent`` (ack delivery) —
        or an ``error`` event if delivery fails. Mirrors the Rust ``offer_otp``.

        A no-op unless all three hold: a tool was refused, an OTP service is installed,
        and the session has a contact to deliver a code to. ``auth_level`` is fixed
        ``end_user`` (the only level this flow remedies); the masked destination +
        channel come from the host — the server never sees the code."""
        tool = refusal.refused_tool
        if tool is None or self._otp_service is None:
            return
        contact = OtpContact(email=session.contact_email)
        if contact.is_empty:
            return
        channels = [c.value for c in contact.available_channels()]
        sink(
            protocol.otp_verification_required(
                request_id,
                tool,
                f"Verify your identity to continue using '{tool}'.",
                channels,
                "end_user",
            )
        )
        try:
            delivery = await self._otp_service.send_otp(session.session_id, contact)
        except Exception as exc:
            sink(protocol.error(request_id, "OTP_SEND_FAILED", f"failed to send verification code: {exc}"))
            return
        sink(protocol.otp_sent(request_id, delivery.channel.value, delivery.masked_destination))

    async def _handle_verify_otp(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        """``verify_otp`` — validate a submitted OTP code and, on success, mark the
        session identity-verified.

        Per ``spec/actions/verify-otp.schema.json`` the client sends
        ``{action, sessionId, requestId, code}`` in reply to an
        ``otp_verification_required`` event. There is no dedicated response event: a
        correct code emits ``otp_verified`` (the client then re-sends its message to
        run the gated tool — the reference server does not park/auto-resume the
        original turn), a rejected code emits ``otp_invalid`` carrying the host's
        remaining-attempt count. Validation order mirrors the Rust handler: requestId
        required, sessionId required, code required, session must exist, then — with no
        :class:`OtpService` installed — fail closed with ``otp_invalid`` (``NOT_FOUND``,
        0 attempts)."""
        # requestId is load-bearing (it echoes the originating
        # otp_verification_required); require it — do NOT auto-generate one.
        if not request_id:
            sink(protocol.error(None, "VALIDATION_ERROR", "verify_otp requires a 'requestId'"))
            return

        session_id = frame.get("sessionId")
        if not session_id:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "verify_otp requires a 'sessionId'"))
            return

        code = frame.get("code")
        if not isinstance(code, str) or not code:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "verify_otp requires a 'code'"))
            return

        # The session must exist (a code can't authenticate a session we don't track).
        session = await self._store.get_session(session_id)
        if session is None:
            sink(protocol.error(request_id, "SESSION_NOT_FOUND", f"session '{session_id}' not found"))
            return

        # No host OTP service → verification is impossible. Fail closed on the
        # documented otp_invalid path (a client shouldn't reach here without first
        # receiving otp_verification_required, which only an installed service emits).
        if self._otp_service is None:
            sink(protocol.otp_invalid(request_id, "NOT_FOUND", 0, "No verification is in progress for this session."))
            return

        outcome = await self._otp_service.verify_otp(session_id, code)
        if isinstance(outcome, OtpVerified):
            await self._store.set_session_authenticated(session_id, True)
            sink(protocol.otp_verified(request_id, "Identity verified successfully."))
        elif isinstance(outcome, OtpInvalid):
            sink(
                protocol.otp_invalid(
                    request_id,
                    outcome.error.value if outcome.error is not None else None,
                    outcome.attempts_remaining,
                    outcome.message,
                )
            )

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


#: Default cap for list_conversations when the caller doesn't ask for a specific limit.
_DEFAULT_LIST_LIMIT = 50

#: Max characters in a conversation title preview before it's clipped with an ellipsis.
_TITLE_MAX = 60

#: Leading markdown/control markers stripped off a preview so a raw message body renders
#: as clean text (heading #, bullets *-, quote >, emphasis _~, code `, cursor ▎). Only
#: LEADING chars are touched — inline markdown mid-text is left alone.
_LEADING_MARKUP = "#*>-_~`▎ "


def _conversation_title(first_inbound_text: str | None, fallback: str) -> str:
    """Derive a ``list_conversations`` entry title from the first inbound message text,
    falling back to ``fallback`` (a conversation name) when there is none. The preview is
    cleaned (leading markdown/control chars stripped) and clipped to ``_TITLE_MAX`` chars
    with an ellipsis. Mirrors the Rust ``conversation_title`` + ``truncate_preview``, plus
    the contract's leading-markdown strip (matching Go/TS). th-d5b446."""
    cleaned = _strip_leading_markup(first_inbound_text or "")
    if not cleaned:
        return fallback
    return _truncate_preview(cleaned, _TITLE_MAX)


def _strip_leading_markup(s: str) -> str:
    """Trim leading whitespace, control chars, and markdown markers off a preview so
    ``"### Hi"`` / ``"- do X"`` title as ``"Hi"`` / ``"do X"``."""
    i = 0
    for ch in s:
        if ch.isspace() or (ch.isprintable() is False) or ch in _LEADING_MARKUP:
            i += 1
        else:
            break
    return s[i:].strip()


def _truncate_preview(s: str, max_chars: int) -> str:
    """Trim ``s`` and clip it to ``max_chars`` (char-safe), appending an ellipsis when
    clipped. Mirrors the Rust ``truncate_preview``."""
    s = s.strip()
    if len(s) <= max_chars:
        return s
    return s[:max_chars].rstrip() + "…"
