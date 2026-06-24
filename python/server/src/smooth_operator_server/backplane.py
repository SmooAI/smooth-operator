"""Connection backplane seam.

For the MVP this is an in-memory stub: it tracks attached connections so a
``detach(conn_id)`` can be run after each connection loop exits (the spec's
detach-after-loop). The Redis/NATS cross-pod fan-out the Rust server supports
(``RedisBackplane`` / ``NatsBackplane``) is left as a seam — a real backplane
implements the same :meth:`attach` / :meth:`detach` surface.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from threading import Lock


class Backplane(ABC):
    """Tracks connection registration so events can be routed to a socket and a
    deregister can run on teardown."""

    @abstractmethod
    async def attach(self, conn_id: str) -> None: ...

    @abstractmethod
    async def detach(self, conn_id: str) -> None: ...


class InMemoryBackplane(Backplane):
    """Single-process backplane — registers connection ids in a set. The reference
    backplane (no cross-pod fan-out)."""

    def __init__(self) -> None:
        self._gate = Lock()
        self._connections: set[str] = set()

    async def attach(self, conn_id: str) -> None:
        with self._gate:
            self._connections.add(conn_id)

    async def detach(self, conn_id: str) -> None:
        with self._gate:
            self._connections.discard(conn_id)

    @property
    def attached_count(self) -> int:
        """How many connections are currently attached (for tests/diagnostics)."""
        with self._gate:
            return len(self._connections)
