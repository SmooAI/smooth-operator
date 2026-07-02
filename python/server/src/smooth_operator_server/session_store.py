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


#: The reference agent's display name (mirrors the Rust ``AGENT_NAME`` and the C#
#: ``InMemorySessionStore`` default).
AGENT_NAME = "smooth-agent"


class SessionStore(ABC):
    """Persistence for sessions + conversation message logs (async, like the Rust
    adapter and the C# ``ISessionStore``)."""

    @abstractmethod
    async def create_session(self, agent_id: str, user_name: str | None, user_email: str | None) -> StoredSession: ...

    @abstractmethod
    async def get_session(self, session_id: str) -> StoredSession | None: ...

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
        #: Per-conversation workflow-step pointer (absent = fresh start / no workflow).
        self._current_step: dict[str, str] = {}
        #: Per-session OTP-verified bit (absent/False = unverified). Set by a
        #: successful ``verify_otp``; read by the ``end_user`` auth gate.
        self._authenticated: dict[str, bool] = {}

    async def create_session(self, agent_id: str, user_name: str | None, user_email: str | None) -> StoredSession:
        session = StoredSession(
            session_id=str(uuid.uuid4()),
            conversation_id=str(uuid.uuid4()),
            agent_id=agent_id if agent_id else str(uuid.uuid4()),
            agent_name=AGENT_NAME,
            user_participant_id=str(uuid.uuid4()),
            agent_participant_id=str(uuid.uuid4()),
            contact_email=(user_email.strip() or None) if isinstance(user_email, str) else None,
        )
        with self._gate:
            self._sessions[session.session_id] = session
            self._messages[session.conversation_id] = []
        return session

    async def get_session(self, session_id: str) -> StoredSession | None:
        with self._gate:
            return self._sessions.get(session_id)

    async def append_message(self, conversation_id: str, direction: MessageDirection, text: str) -> StoredMessage:
        message = StoredMessage(str(uuid.uuid4()), conversation_id, direction, text)
        with self._gate:
            self._messages.setdefault(conversation_id, []).append(message)
        return message

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
