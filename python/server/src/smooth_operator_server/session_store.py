"""Session + conversation-message persistence.

The Python analog of the C# ``ISessionStore`` / ``InMemorySessionStore`` and the
Rust storage adapter's session/message surface. The protocol's
``create_conversation_session`` / ``get_session`` operate on sessions; a turn
appends to (and replays) the conversation message log.

The bundled :class:`InMemorySessionStore` is the reference store (lost on restart);
the abstract :class:`SessionStore` is the seam a durable (Postgres/Dynamo) adapter
would implement.
"""

from __future__ import annotations

import uuid
from abc import ABC, abstractmethod
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from threading import Lock


@dataclass(frozen=True)
class StoredSession:
    """A conversation session — the unit create/get operate on."""

    session_id: str
    conversation_id: str
    agent_id: str
    agent_name: str
    user_participant_id: str
    agent_participant_id: str
    #: The caller's email captured at create time, used as the OTP delivery contact
    #: for the ``end_user`` identity flow (the Python analog of the Rust session's
    #: ``metadata.contactEmail``). ``None`` when no email was supplied — the server
    #: then can't offer OTP for this session.
    contact_email: str | None = None


class MessageDirection(Enum):
    """Who a message came from."""

    INBOUND = "inbound"  # from the user
    OUTBOUND = "outbound"  # from the agent


@dataclass(frozen=True)
class StoredMessage:
    """One logged conversation message."""

    id: str
    conversation_id: str
    direction: MessageDirection
    text: str


@dataclass(frozen=True)
class ConversationSummary:
    """One conversation's roll-up for the ``list_conversations`` action — enough to
    render a resumable-thread list without pulling every message. The dispatcher turns
    ``first_inbound_text`` / ``updated_at`` / ``message_count`` into the wire
    ``{conversationId, title, updatedAt, messageCount}``. The Python analog of the Go
    ``ConversationSummary`` and the TS ``ConversationSummary``. th-d5b446."""

    conversation_id: str
    #: Last-activity timestamp (create, then every append) — the sort key + ``updatedAt``.
    updated_at: datetime
    #: Total messages in the conversation. The dispatcher drops empties (``0``).
    message_count: int
    #: Text of the FIRST inbound (user) message — the dispatcher's title source.
    #: ``None`` when the conversation has no inbound message (title falls back to a name).
    first_inbound_text: str | None = None


#: The reference agent's display name (mirrors the Rust ``AGENT_NAME`` and the C#
#: ``InMemorySessionStore`` default).
AGENT_NAME = "smooth-agent"


class SessionStore(ABC):
    """Persistence for sessions + conversation message logs (async, like the Rust
    adapter and the C# ``ISessionStore``)."""

    @abstractmethod
    async def create_session(
        self, agent_id: str, user_name: str | None, user_email: str | None, conversation_id: str | None = None
    ) -> StoredSession:
        """Mint a session. When ``conversation_id`` names an EXISTING conversation, the
        new session binds to it (resume: reuses the id + its persisted message log, so
        subsequent turns append and history replays). An absent or unknown id mints a
        fresh conversation (unchanged behavior). th-d5b446."""
        ...

    @abstractmethod
    async def get_session(self, session_id: str) -> StoredSession | None: ...

    @abstractmethod
    async def list_conversations(self) -> list[ConversationSummary]:
        """A summary per conversation that has at least one message (empty conversations —
        every page-load currently mints one — are dropped), in no particular order; the
        dispatcher sorts most-recent-first and caps. The Python analog of the Rust
        ``list_conversations_by_org`` + per-conversation peek. th-d5b446."""
        ...

    @abstractmethod
    async def append_message(self, conversation_id: str, direction: MessageDirection, text: str) -> StoredMessage: ...

    @abstractmethod
    async def list_messages(self, conversation_id: str, limit: int) -> list[StoredMessage]:
        """The most recent ``limit`` messages for a conversation, oldest first."""
        ...

    @abstractmethod
    async def get_current_step_id(self, conversation_id: str) -> str | None:
        """The conversation's current workflow-step pointer (``None`` = fresh start)."""
        ...

    @abstractmethod
    async def set_current_step_id(self, conversation_id: str, step_id: str | None) -> None:
        """Persist the conversation's workflow-step pointer (the analog of the TS
        ``state.currentStepId`` carried across turns)."""
        ...

    @abstractmethod
    async def is_session_authenticated(self, session_id: str) -> bool:
        """Whether the caller has completed OTP identity verification for this session
        (the Python analog of the Rust session's ``metadata.otpVerified``). ``False``
        for an unknown or unverified session. Threaded into the ``end_user`` auth gate
        so a verified session's gated tools run."""
        ...

    @abstractmethod
    async def set_session_authenticated(self, session_id: str, verified: bool) -> None:
        """Mark this session identity-verified (or clear it). Called after a
        successful ``verify_otp``. A no-op for an unknown session."""
        ...


class InMemorySessionStore(SessionStore):
    """In-process :class:`SessionStore` — the reference store (the C# analog of
    ``InMemorySessionStore`` / the Rust in-memory adapter). A lock guards the dicts
    so concurrent connections never corrupt them."""

    def __init__(self) -> None:
        self._gate = Lock()
        self._sessions: dict[str, StoredSession] = {}
        self._messages: dict[str, list[StoredMessage]] = {}
        #: Per-conversation last-activity time (create, then every append) — the sort key
        #: + updated_at source for list_conversations. th-d5b446.
        self._updated_at: dict[str, datetime] = {}
        #: Per-conversation workflow-step pointer (absent = fresh start / no workflow).
        self._current_step: dict[str, str] = {}
        #: Per-session OTP-verified bit (absent/False = unverified). Set by a
        #: successful ``verify_otp``; read by the ``end_user`` auth gate.
        self._authenticated: dict[str, bool] = {}

    async def create_session(
        self, agent_id: str, user_name: str | None, user_email: str | None, conversation_id: str | None = None
    ) -> StoredSession:
        with self._gate:
            # Resume: bind to an existing conversation (reuse its id + persisted log) when
            # the caller passes a known conversationId. Unknown/absent → mint a fresh one.
            resume = bool(conversation_id) and conversation_id in self._messages
            conv_id = conversation_id if resume else str(uuid.uuid4())
            session = StoredSession(
                session_id=str(uuid.uuid4()),
                conversation_id=conv_id,
                agent_id=agent_id if agent_id else str(uuid.uuid4()),
                agent_name=AGENT_NAME,
                user_participant_id=str(uuid.uuid4()),
                agent_participant_id=str(uuid.uuid4()),
                contact_email=(user_email.strip() or None) if isinstance(user_email, str) else None,
            )
            self._sessions[session.session_id] = session
            # Only seed an empty log + timestamp on a fresh conversation — a resume keeps
            # its history (and prior last-activity time).
            if not resume:
                self._messages[conv_id] = []
                self._updated_at[conv_id] = datetime.now(timezone.utc)
        return session

    async def get_session(self, session_id: str) -> StoredSession | None:
        with self._gate:
            return self._sessions.get(session_id)

    async def append_message(self, conversation_id: str, direction: MessageDirection, text: str) -> StoredMessage:
        message = StoredMessage(str(uuid.uuid4()), conversation_id, direction, text)
        with self._gate:
            self._messages.setdefault(conversation_id, []).append(message)
            self._updated_at[conversation_id] = datetime.now(timezone.utc)
        return message

    async def list_conversations(self) -> list[ConversationSummary]:
        with self._gate:
            out: list[ConversationSummary] = []
            for conv_id, log in self._messages.items():
                if not log:  # drop empties — every page-load mints one
                    continue
                # Messages are stored oldest-first, so the first inbound is the title source.
                first_inbound = next((m.text for m in log if m.direction is MessageDirection.INBOUND), None)
                out.append(
                    ConversationSummary(
                        conversation_id=conv_id,
                        updated_at=self._updated_at.get(conv_id, datetime.now(timezone.utc)),
                        message_count=len(log),
                        first_inbound_text=first_inbound,
                    )
                )
            return out

    async def list_messages(self, conversation_id: str, limit: int) -> list[StoredMessage]:
        with self._gate:
            log = self._messages.get(conversation_id, [])
            return list(log[-limit:]) if limit > 0 else list(log)

    async def get_current_step_id(self, conversation_id: str) -> str | None:
        with self._gate:
            return self._current_step.get(conversation_id)

    async def set_current_step_id(self, conversation_id: str, step_id: str | None) -> None:
        with self._gate:
            if step_id is None:
                self._current_step.pop(conversation_id, None)
            else:
                self._current_step[conversation_id] = step_id

    async def is_session_authenticated(self, session_id: str) -> bool:
        with self._gate:
            return self._authenticated.get(session_id, False)

    async def set_session_authenticated(self, session_id: str, verified: bool) -> None:
        with self._gate:
            # Only a tracked session can be verified — mirrors the Rust no-op for an
            # unknown session (a code can't authenticate something we don't hold).
            if session_id not in self._sessions:
                return
            if verified:
                self._authenticated[session_id] = True
            else:
                self._authenticated.pop(session_id, None)
