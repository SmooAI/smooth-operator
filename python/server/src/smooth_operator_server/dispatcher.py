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
import logging
import uuid
from datetime import timezone
from typing import Any, Awaitable, Callable

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
from .auth import AccessContext, normalize_email
from .backplane import Target
from .confirmation import ConfirmationRegistry
from .interaction import InteractionOutcome, InteractionRegistry, PendingInteractions
from .otp import OtpContact, OtpInvalid, OtpService, OtpVerified
from .session_store import SessionStore
from .turn_runner import Sink, TurnContext, TurnRunner

#: An ``INTERNAL_ERROR`` on the wire must leave a traceback behind here. The wire message stays
#: generic (no detail leaks to the client), but a server whose every turn fails has to say WHY
#: somewhere, or the failure is undiagnosable — th-e7ef23.
_log = logging.getLogger(__name__)


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
        interactions: InteractionRegistry | None = None,
        interaction_pending: PendingInteractions | None = None,
        agent_config_resolver: AgentConfigResolver | None = None,
        session_authenticator: SessionAuthenticator | None = None,
        judge_model: str | None = None,
        otp_service: OtpService | None = None,
        associate: Callable[[Target], Awaitable[None]] | None = None,
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
        #: Rich Interactions catalog (default: the reference ``choices`` kind) + the
        #: session-keyed park registry. A turn registers the per-kind raise tools; a
        #: ``submit_interaction`` frame resolves the park a raise tool awaits — always on
        #: this same connection, so (like confirmations) the registry is connection-local.
        self._interactions = interactions if interactions is not None else InteractionRegistry.default()
        self._interaction_pending = interaction_pending if interaction_pending is not None else PendingInteractions()
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
        #: Backplane association hook. None in tests/hosts with no backplane wiring,
        #: which simply means session/agent targets are never routable there.
        self._associate = associate
        #: Fast model for the post-turn workflow judge (None → runner's default).
        self._judge_model = judge_model
        #: Spawned turn tasks kept alive (the event loop only holds weak refs to
        #: tasks, and a cancelled turn still needs to run its cancellation to
        #: completion); cleared as each finishes so they don't accumulate.
        self._turn_tasks: set[asyncio.Task[Any]] = set()
        #: The connection's single active agent turn, if one is running: the turn's
        #: ``requestId`` (echoed on the ``cancelled`` event) + its task. Kept as a
        #: strong ref (the loop only holds weak ones) and cleared when the turn
        #: completes. Only ONE turn runs at a time — a second ``send_message`` while
        #: one is in flight is rejected with ``TURN_IN_PROGRESS``, never run
        #: concurrently. Mirrors the Rust reader loop's ``current_turn`` slot.
        self._active_turn: tuple[str | None, asyncio.Task[Any]] | None = None

    def _in_flight(self) -> tuple[str | None, asyncio.Task[Any]] | None:
        """The active turn, or ``None`` if there is none / it already finished. The
        done check mirrors the Rust ``handle.is_finished()`` guard: the completion
        callback that clears the slot is scheduled, not immediate, so a frame arriving
        in that window must not see a finished turn as in-flight."""
        if self._active_turn is not None and self._active_turn[1].done():
            self._active_turn = None
        return self._active_turn

    def cancel_active_turn(self, sink: Sink | None = None) -> bool:
        """Abort the connection's in-flight turn, if any, and (with a ``sink``) emit
        the terminal ``cancelled`` event echoing the cancelled turn's ``requestId``.

        Cancelling the task raises :class:`asyncio.CancelledError` inside the turn at
        its next ``await``, abandoning the in-flight LLM/tool call. The partial
        assistant reply is discarded — it is persisted only at the END of the turn,
        which the cancellation skips; the user's message was persisted at the start,
        so it stays. Returns whether a turn was actually aborted (a cancel with none
        running is a harmless no-op that emits nothing)."""
        active = self._in_flight()
        if active is None:
            return False
        turn_request_id, task = active
        self._active_turn = None
        task.cancel()
        if sink is not None:
            sink(protocol.cancelled(turn_request_id))
        return True

    async def wait_for_turns(self) -> None:
        """Await every spawned ``send_message`` turn to completion (including one
        already cancelled, whose cancellation still has to unwind).

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
            elif action == "get_conversation_messages":
                await self._handle_get_conversation_messages(frame, request_id, sink)
            elif action == "get_session":
                await self._handle_get_session(frame, request_id, sink)
            elif action == "cancel":
                # Client-initiated turn cancellation (the "Stop button"). Connection-
                # local: aborts the tracked turn task and emits `cancelled`. With no
                # turn running it is a silent no-op.
                self.cancel_active_turn(sink)
            elif action == "send_message":
                if self._in_flight() is not None:
                    # A turn is already running on this connection: reject the second
                    # one rather than run two concurrently (interleaved streams +
                    # racing storage writes). The client must cancel it or wait.
                    sink(
                        protocol.error(
                            request_id,
                            "TURN_IN_PROGRESS",
                            "a turn is already in progress on this connection; cancel it or wait for it to complete",
                        )
                    )
                else:
                    await self._handle_send_message(frame, request_id, sink)
            elif action == "confirm_tool_action":
                self._handle_confirm_tool_action(frame, request_id, sink)
            elif action == "submit_interaction":
                await self._handle_submit_interaction(frame, request_id, sink)
            elif action == "verify_otp":
                await self._handle_verify_otp(frame, request_id, sink)
            elif action is None:
                sink(protocol.error(request_id, "VALIDATION_ERROR", "missing 'action' field"))
            else:
                sink(protocol.error(request_id, "UNSUPPORTED_ACTION", f"action '{action}' is not supported"))
        except Exception:
            # A handler failed mid-turn. Emit a clean error and KEEP the connection
            # alive — never drop the socket with no signal (exception detail stays
            # server-side, not leaked over the wire — but LOGGED, see th-e7ef23).
            # Mirrors the C#/Rust handler.
            _log.exception("INTERNAL_ERROR handling action %r (requestId %r)", action, request_id)
            sink(protocol.error(request_id, "INTERNAL_ERROR", "Internal error processing the request."))

    def _scope_email(self) -> str | None:
        """The email this connection's conversations are scoped to.

        ``None`` means "no owner to match": either auth is disabled (the single-tenant
        local flavor, where ownership is not consulted at all — see
        :meth:`_auth_enforced`) or the principal is authenticated but carries no email
        claim. In the latter case it matches no OWNED conversation, only ownerless ones.
        th-8fe998, th-909995."""
        return None if self._access.auth_disabled else self._access.scope_email

    @property
    def _auth_enforced(self) -> bool:
        return not self._access.auth_disabled

    async def _visible_session(self, session_id: str) -> Any:
        """Resolve a visible session and record its backplane targets.

        Association lives here rather than in each handler because this is already the
        single funnel every sessionId-bearing action goes through — so one hook covers
        them all, and a handler can never work with a session the backplane does not
        know about."""
        session = await self._resolve_visible_session(session_id)
        if session is not None:
            await self._associate_session(session)
        return session

    async def _associate_session(self, session: Any) -> None:
        """Point the session (and its agent) at this connection, so a publish to either
        target reaches this socket. No-op without a backplane hook."""
        if self._associate is None:
            return
        await self._associate(Target("session", session.session_id))
        if session.agent_id:
            await self._associate(Target("agent", session.agent_id))

    async def _resolve_visible_session(self, session_id: str) -> Any:
        """The session, but only if this connection's principal may see it — otherwise
        ``None``, exactly as for a session id that never existed.

        An OWNED session requires an exact (case/whitespace-insensitive) match against
        the principal's email. An OWNERLESS session — minted by an anonymous or
        emailless principal, or predating ownership — stays visible to everyone, as it
        was before scoping shipped. Refusing those instead locked anonymous and
        emailless principals out of the sessions they had just created themselves: empty
        list, no resume, no send_message, i.e. no product. th-909995.

        This is the ONLY permitted way to turn a client-supplied sessionId into a
        session. Every sessionId-bearing action routes through here so one guard covers
        them all — calling ``self._store.get_session`` directly from a handler bypasses
        the ownership check and is a cross-user data leak (th-1b7ed0 was exactly that,
        in ``get_conversation_messages``). ``test_visible_session_is_the_only_lookup``
        fails if a second direct call site appears.

        Callers MUST report a miss with the same ``SESSION_NOT_FOUND`` payload they use
        for a genuinely unknown id: a distinct "forbidden" (or a differently-worded
        message) turns this endpoint into an existence oracle for enumerating other
        users' session ids. th-8fe998."""
        session = await self._store.get_session(session_id)
        if session is None:
            return None
        if not self._auth_enforced:
            return session  # auth disabled → single tenant, no ownership to check
        # Org is the OUTER scope, checked BEFORE ownership: another org's conversation
        # is unreachable no matter who owns it. Without this the ownerless branch below
        # (deliberate, th-909995) let any principal read any org's ownerless
        # conversation given only its id — authorization resting on an unguessable
        # UUID. An unrecorded org (rows predating org capture) is not org-scoped, so
        # those stay reachable by their owner rather than being locked away.
        if session.owner_org is not None and session.owner_org != self._access.principal.org:
            return None
        owner = normalize_email(session.owner_email)
        if owner is None:
            return session  # ownerless → open within the org (th-909995)
        # Owned → exact match only. An emailless principal (scope None) matches no
        # owner, so it still cannot reach anyone else's owned session.
        return session if owner == self._scope_email() else None

    async def _handle_create_session(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        # Resume: a `conversationId` naming an existing conversation binds the new session
        # to it (reuses id + history); absent/unknown → a fresh conversation. The response
        # echoes conversationId either way, so a resuming client sees the id it passed.
        # Mirrors the Rust/Go/TS reference. th-d5b446.
        # agentId is REQUIRED by the Request schema and the generated client type is
        # non-optional, so absent-or-blank is a MALFORMED REQUEST, not an agentless session.
        # The old code fabricated a UUID and th-68897a's first pass silently stored NULL —
        # both skip the validation that belongs at this boundary. The column stays nullable
        # for rows written before this check; it is just no longer reachable from here.
        agent_id = str(frame.get("agentId") or "").strip()
        if not agent_id:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "Missing 'agentId'"))
            return

        raw_conv_id = frame.get("conversationId")
        conversation_id = raw_conv_id if isinstance(raw_conv_id, str) and raw_conv_id else None
        # Ownership comes from the AUTHENTICATED PRINCIPAL, never from the frame. The
        # client-supplied `userEmail` is exactly the spoofing vector this fix closes:
        # honoring it would let anyone claim (and then list) another user's scope. With
        # auth on, the principal's email also replaces `userEmail` as the OTP contact,
        # so a code is never delivered to a client-chosen address. th-8fe998.
        owner_email = self._scope_email()
        user_email = owner_email if self._auth_enforced else frame.get("userEmail")
        session = await self._store.create_session(
            agent_id,
            frame.get("userName"),
            user_email,
            conversation_id,
            owner_email=owner_email,
            enforced=self._auth_enforced,
            org_id=self._access.principal.org,
        )
        await self._associate_session(session)
        # Capture the declared render capabilities (``supports``) that gate Rich
        # Interactions per turn. Unknown values are kept as-is and simply never match a
        # kind's capability (forward-compatible).
        #
        # They are persisted on the CONVERSATION, not this connection: a RECONNECT is a
        # resume — the client re-opens the socket and re-issues this action with the same
        # ``conversationId``, minting a NEW session on a NEW dispatcher. Capabilities held
        # per connection were therefore dropped on every network blip / backgrounding /
        # deploy, and every interaction kind quietly fell back to conversational
        # collection with nothing on the wire to notice (th-13df6d).
        #
        # A frame that DECLARES always wins and overwrites, including an explicit ``[]``:
        # that is how a text-only channel resuming a rich conversation opts out, and the
        # opt-out has to be durable too — hence a list, empty or not, is always written.
        # A frame that OMITS the key writes nothing and so inherits whatever the
        # conversation last declared (a non-list is treated as omitted, mirroring the Rust
        # reference). A fresh conversation has nothing stored, so omitting still means
        # text-only — unchanged behavior.
        raw_supports = frame.get("supports")
        if isinstance(raw_supports, list):
            await self._store.set_client_supports(
                session.conversation_id, [s for s in raw_supports if isinstance(s, str)]
            )
        data = {
            "sessionId": session.session_id,
            "conversationId": session.conversation_id,
            "agentName": session.agent_name,
            "userParticipantId": session.user_participant_id,
            "agentParticipantId": session.agent_participant_id,
        }
        # Omitted rather than null when the session has no agent — absent is the honest
        # wire shape, and a fabricated id was the bug (th-68897a).
        if session.agent_id:
            data["agentId"] = session.agent_id
        sink(protocol.immediate_response(request_id, 200, "Session created", data))

    async def _handle_get_session(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        session_id = frame.get("sessionId")
        if not session_id:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "missing 'sessionId'"))
            return
        # Not-yours is reported identically to not-found — no existence oracle.
        session = await self._visible_session(session_id)
        if session is None:
            sink(protocol.error(request_id, "SESSION_NOT_FOUND", f"session '{session_id}' not found"))
            return
        data = {
            "sessionId": session.session_id,
            "conversationId": session.conversation_id,
            "agentName": session.agent_name,
            "userParticipantId": session.user_participant_id,
            "agentParticipantId": session.agent_participant_id,
        }
        # Omitted rather than null when the session has no agent — absent is the honest
        # wire shape, and a fabricated id was the bug (th-68897a).
        if session.agent_id:
            data["agentId"] = session.agent_id
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

        # Scope to the authenticated principal: their own conversations plus ownerless
        # ones (th-909995 — an emailless/anonymous principal must still see the
        # conversations it created, which are ownerless by construction). The store
        # applies the filter in its selection, so `limit` below caps an already-scoped
        # list rather than paging over other users' rows. th-8fe998.
        summaries = await self._store.list_conversations(
            self._scope_email(), enforced=self._auth_enforced, org_id=self._access.principal.org
        )
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

    async def _handle_get_conversation_messages(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        """``get_conversation_messages`` — the conversation-resume substrate. Returns a
        session's messages NEWEST-first plus ``nextCursor``/``hasMore``, per
        ``spec/actions/get-messages.schema.json``: each entry is ``{id, direction,
        content: {text}, createdAt}``. Optional input: ``limit`` (1..100, default 50) and
        ``cursor`` (opaque — a message id today; returns the messages older than the one
        it names). Page by feeding a response's ``nextCursor`` back as the next request's
        ``cursor``. Deliberately NOT a timestamp cursor: messages can share a
        ``created_at``, and a ``created_at <`` filter drops or repeats the collisions.
        Mirrors the Rust ``handle_get_conversation_messages`` and the Go/TS/C# reference.
        th-89b698, th-ebc251."""
        session_id = frame.get("sessionId")
        if not session_id:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "missing 'sessionId'"))
            return
        # Owner-scoped like every other sessionId action — reading someone else's history
        # is reported identically to a session that never existed. th-1b7ed0.
        session = await self._visible_session(session_id)
        if session is None:
            sink(protocol.error(request_id, "SESSION_NOT_FOUND", f"session '{session_id}' not found"))
            return

        raw_limit = frame.get("limit")
        limit = (
            min(raw_limit, _MAX_MESSAGE_LIMIT)
            if isinstance(raw_limit, int) and not isinstance(raw_limit, bool) and raw_limit > 0
            else _DEFAULT_LIST_LIMIT
        )

        raw_cursor = frame.get("cursor")
        cursor = raw_cursor if isinstance(raw_cursor, str) and raw_cursor else None

        # No cursor: limit+1 rows, where the extra row means "more history exists". With a
        # cursor we have to locate the named message first, so read the log.
        # ponytail: cursor paging reads the whole conversation log and slices in memory.
        # Push it into a cursor-aware store read if conversations outgrow that —
        # SessionStore is implemented downstream, so widening it is a breaking change.
        stored = await self._store.list_messages(session.conversation_id, _FULL_LOG if cursor else limit + 1)

        # The store returns oldest-first; the contract is newest-first.
        newest_first = list(reversed(stored))
        if cursor is not None:
            at = next((i for i, m in enumerate(newest_first) if m.id == cursor), None)
            if at is None:
                sink(
                    protocol.error(request_id, "VALIDATION_ERROR", f"unknown 'cursor' '{cursor}' for this conversation")
                )
                return
            newest_first = newest_first[at + 1 :]

        has_more = len(newest_first) > limit
        page = newest_first[:limit]
        messages = [
            {
                "id": m.id,
                "direction": m.direction.value,
                "content": {"text": m.text},
                "createdAt": m.created_at.astimezone(timezone.utc).isoformat(),
            }
            for m in page
        ]
        # `nextCursor` names the OLDEST message in this page — non-null iff `hasMore`.
        sink(
            protocol.immediate_response(
                request_id,
                200,
                "ConversationMessages",
                {"messages": messages, "nextCursor": page[-1].id if has_more else None, "hasMore": has_more},
            )
        )

    async def _handle_send_message(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        # requestId is load-bearing for streaming correlation; generate one if the
        # client omitted it (mirrors the C# `requestId ??= Guid`).
        if not request_id:
            request_id = str(uuid.uuid4())

        session_id = frame.get("sessionId")
        if not session_id:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "missing 'sessionId'"))
            return

        session = await self._visible_session(session_id)
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
        # An agentless session has nothing to resolve — don't ask the resolver about an
        # agent that does not exist (th-68897a).
        agent_config = await self._agent_config_resolver.resolve(session.agent_id) if session.agent_id else None
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
        # Multimodal / file-transfer context (spec PR #342): parse the turn's optional
        # image + file attachments fail-soft (a malformed entry is dropped, never fails
        # the turn — per the schema) and carry them on the per-turn TurnContext. Images
        # reach the model as `image_url` parts; files are surfaced for a host tool to
        # land into the workspace; the context's directive sink lets a host tool hand a
        # client-side directive (e.g. `send_file`) back onto `eventual_response`.
        turn_context = TurnContext(
            images=_parse_attachments(frame.get("images"), ("url",)),
            files=_parse_attachments(frame.get("files"), ("name", "url")),
        )
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
            org_id=self._access.principal.org,
            interactions=self._interactions,
            interaction_pending=self._interaction_pending,
            # Read from the conversation, the one source of truth: this turn may be
            # running on a reconnect whose frame never re-declared `supports` (th-13df6d).
            capabilities=await self._store.get_client_supports(session.conversation_id),
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
                    session.conversation_id,
                    request_id_str,
                    message,
                    sink,
                    session_id=session_id_str,
                    context=turn_context,
                )
            except Exception:
                # Mirror the dispatcher's outer guard: a turn failure surfaces a clean
                # error and keeps the connection alive (detail stays server-side — but
                # LOGGED, see th-e7ef23).
                _log.exception("INTERNAL_ERROR running turn (requestId %r)", request_id_str)
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
                    usage=result.usage,
                    directive=result.directive,
                )
            )

        task = asyncio.ensure_future(_run_turn())
        # Track it as the connection's single active turn so a later `cancel` frame
        # (or a disconnect) can abort it, and a second `send_message` is rejected.
        self._active_turn = (request_id_str, task)
        self._turn_tasks.add(task)
        task.add_done_callback(self._turn_tasks.discard)
        task.add_done_callback(self._clear_active_turn)

    def _clear_active_turn(self, task: asyncio.Task[Any]) -> None:
        """Free the active-turn slot once its task finishes (only if it is still THIS
        task's — a cancel already took the slot, and a later turn may own it now)."""
        if self._active_turn is not None and self._active_turn[1] is task:
            self._active_turn = None

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
        # Both contacts feed the OTP seam: the pre-chat email, plus a phone captured by an
        # identity_intake submit (attach_session_identity) — so a captured contact is
        # immediately OTP-verifiable over whichever channel it filled (email and/or SMS).
        contact = OtpContact(email=session.contact_email, phone=session.contact_phone)
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

        # The session must exist AND belong to this principal (a code can't authenticate
        # a session we don't track — or someone else's).
        session = await self._visible_session(session_id)
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

    async def _handle_submit_interaction(self, frame: dict, request_id: str | None, sink: Sink) -> None:
        """``submit_interaction`` — the single Rich Interactions resume verb: resolve a
        turn parked on a raised interaction with the visitor's values (or ``declined``).

        Per ``spec/actions/submit-interaction.schema.json`` the client replies with
        ``{action, sessionId, requestId, interactionId, kind?, values?, declined?}`` to an
        ``interaction_required`` event. Flow mirrors the Rust ``handle_submit_interaction``
        exactly: require ``requestId`` + ``sessionId`` + ``interactionId``; the session
        must be visible; PEEK the park (don't consume) so an invalid submit leaves the
        turn parked; the ``interactionId`` (and ``kind``, if given) must match the park
        (else ``INTERACTION_MISMATCH``, so a stale card can't resolve a newer park). A
        decline resolves with a declined payload; values route to the kind's validator —
        invalid values emit ``interaction_invalid`` and KEEP the turn parked (retryable,
        never a terminal ``error``, like ``otp_invalid``); valid values resolve the park
        with the canonical payload. Continuation is signalled by the resumed streaming
        sequence; we ack with an ``immediate_response``."""
        # requestId echoes the originating interaction_required — require it, don't invent.
        if not request_id:
            sink(protocol.error(None, "VALIDATION_ERROR", "submit_interaction requires a 'requestId'"))
            return
        session_id = frame.get("sessionId")
        if not session_id:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "submit_interaction requires a 'sessionId'"))
            return
        interaction_id = frame.get("interactionId")
        if not interaction_id:
            sink(protocol.error(request_id, "VALIDATION_ERROR", "submit_interaction requires an 'interactionId'"))
            return

        # Not-yours is reported identically to not-found — no existence oracle.
        session = await self._visible_session(session_id)
        if session is None:
            sink(protocol.error(request_id, "SESSION_NOT_FOUND", f"session '{session_id}' not found"))
            return

        # Peek (don't consume): an invalid submit must leave the turn parked for a resubmit.
        pending = self._interaction_pending.peek(session_id)
        if pending is None:
            sink(
                protocol.error(
                    request_id,
                    "NO_PENDING_INTERACTION",
                    f"no interaction is awaiting a submit for session '{session_id}'",
                )
            )
            return
        if interaction_id != pending.interaction_id:
            sink(
                protocol.error(
                    request_id,
                    "INTERACTION_MISMATCH",
                    "interactionId does not match the parked interaction",
                )
            )
            return
        claimed_kind = frame.get("kind")
        if isinstance(claimed_kind, str) and claimed_kind and claimed_kind != pending.kind:
            sink(protocol.error(request_id, "INTERACTION_MISMATCH", "kind does not match the parked interaction"))
            return

        # Decline: resume the turn with a declined payload so the agent proceeds gracefully.
        if frame.get("declined") is True:
            self._interaction_pending.resolve(session_id, InteractionOutcome.declined())
            sink(protocol.immediate_response(request_id, 200, "Interaction declined", {"sessionId": session_id}))
            return

        values = frame.get("values")
        if not isinstance(values, dict):
            sink(
                protocol.error(
                    request_id, "VALIDATION_ERROR", "submit_interaction requires 'values' unless 'declined' is true"
                )
            )
            return

        kind = self._interactions.get(pending.kind)
        if kind is None:  # a hosted-kind park whose kind was since removed — defensive.
            sink(protocol.error(request_id, "NO_PENDING_INTERACTION", f"unknown interaction kind '{pending.kind}'"))
            return
        canonical, errors = kind.validate(pending.spec, values)
        if errors:
            # Retryable: re-render the card with the per-field errors; the turn stays parked.
            sink(
                protocol.interaction_invalid(
                    request_id,
                    pending.interaction_id,
                    pending.kind,
                    [e.to_dict() for e in errors],
                    "Some fields need attention.",
                )
            )
            return

        # Valid: run the kind's host effect (e.g. identity_intake stamps the captured contacts
        # onto the session), THEN consume the park and resume the turn with the canonical payload.
        # The effect runs before the resume, mirroring the Rust handle_submit_interaction; it is a
        # no-op for kinds without one (choices), so this stays kind-agnostic.
        await kind.host_effect(self._store, session_id, canonical or {})
        self._interaction_pending.resolve(session_id, InteractionOutcome.submitted(canonical or {}))
        sink(
            protocol.immediate_response(
                request_id,
                200,
                "Interaction submitted",
                {"sessionId": session_id, "interactionId": pending.interaction_id, "values": canonical},
            )
        )


#: Default cap for list_conversations when the caller doesn't ask for a specific limit.
_DEFAULT_LIST_LIMIT = 50

#: Contract cap on get_conversation_messages' `limit` (1..100). th-89b698.
_MAX_MESSAGE_LIMIT = 100

#: Sentinel `limit` meaning "the whole conversation log" — the store contract is "the most
#: recent N, oldest first", so this just has to exceed any real conversation. Used when
#: get_conversation_messages has to locate a `cursor` message. th-ebc251.
_FULL_LOG = 1_000_000


#: Max characters in a conversation title preview before it's clipped with an ellipsis.
_TITLE_MAX = 60

#: Leading markdown/control markers stripped off a preview so a raw message body renders
#: as clean text (heading #, bullets *-, quote >, emphasis _~, code `, cursor ▎). Only
#: LEADING chars are touched — inline markdown mid-text is left alone.
_LEADING_MARKUP = "#*>-_~`▎ "


def _parse_attachments(raw: Any, required: tuple[str, ...]) -> list[dict[str, Any]]:
    """Parse a ``send_message`` attachment array (``images`` / ``files``) fail-soft.

    Keeps only object entries carrying every ``required`` key as a non-empty string
    (``("url",)`` for images; ``("name", "url")`` for files) and shallow-copies each so
    the caller owns the dicts. A non-list ``raw`` (absent/``None``/garbage) or a
    malformed entry is dropped rather than rejecting the turn — the fail-soft contract
    the ``images``/``files`` schema mandates. The protocol layer never inspects the
    shape further; the model-attach (images) and host (files) consume it downstream."""
    if not isinstance(raw, list):
        return []
    out: list[dict[str, Any]] = []
    for entry in raw:
        if not isinstance(entry, dict):
            continue
        if all(isinstance(entry.get(k), str) and entry.get(k) for k in required):
            out.append(dict(entry))
    return out


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
