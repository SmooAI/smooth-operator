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


class InMemorySessionStore(SessionStore):
    """In-process :class:`SessionStore` — the reference store (the C# analog of
    ``InMemorySessionStore`` / the Rust in-memory adapter). A lock guards the dicts
    so concurrent connections never corrupt them."""

    def __init__(self) -> None:
        self._gate = Lock()
        self._sessions: dict[str, StoredSession] = {}
        self._messages: dict[str, list[StoredMessage]] = {}

    async def create_session(self, agent_id: str, user_name: str | None, user_email: str | None) -> StoredSession:
        session = StoredSession(
            session_id=str(uuid.uuid4()),
            conversation_id=str(uuid.uuid4()),
            agent_id=agent_id if agent_id else str(uuid.uuid4()),
            agent_name=AGENT_NAME,
            user_participant_id=str(uuid.uuid4()),
            agent_participant_id=str(uuid.uuid4()),
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
