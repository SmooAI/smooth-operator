"""Connection backplane: the scale-out + event-delivery seam for the WebSocket server.

Each connection's outbound sink is :meth:`attach`\\ ed on connect and
:meth:`associate`\\ d with targets (its session / user / org / agent) as they are
learned; :meth:`publish` delivers an event to **every connection for a target**.
That is what lets a non-AI publisher (job status, ingestion progress,
notifications) push to a connected client without going through an agent turn.

Port of the Rust reference's ``rust/smooth-operator/src/backplane.rs``, including
its 5-target fan-out. :class:`InMemoryBackplane` is single-process — it delivers
straight to local sinks. A Redis/NATS impl satisfies the same surface and also
fans out to other pods, whose deliveries this pod's count omits.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from threading import Lock
from typing import Any, Callable, NamedTuple

#: A connection's local delivery sink: hand it an event, it reaches that socket.
LocalSink = Callable[[dict[str, Any]], None]

#: The five target kinds. A publish to anything else is the caller's error.
TARGET_KINDS = ("connection", "session", "user", "org", "agent")


class Target(NamedTuple):
    """A delivery target: one connection, or every connection for a
    session / user / org / agent. Hashable, so it keys the registry directly."""

    kind: str
    id: str


class Backplane(ABC):
    """A per-process sink registry plus event delivery."""

    @abstractmethod
    async def attach(self, conn_id: str, sink: LocalSink) -> None:
        """Register a connection's outbound sink; this process owns the socket.
        Re-attach replaces the sink. Always reachable as ``("connection", conn_id)``."""

    @abstractmethod
    async def detach(self, conn_id: str) -> None:
        """Drop a connection's sink and every association (run on teardown)."""

    @abstractmethod
    async def associate(self, conn_id: str, target: Target) -> None:
        """Associate a connection with a target. Idempotent; learned over the
        connection's life (user/org from auth, session/agent as sessions resolve)."""

    @abstractmethod
    async def publish(self, target: Target, event: dict[str, Any]) -> int:
        """Deliver ``event`` to every connection for ``target``, returning the count
        of **local** deliveries. Never claims a delivery that did not happen."""


class InMemoryBackplane(Backplane):
    """Single-process :class:`Backplane` — local registry, direct local delivery."""

    def __init__(self) -> None:
        self._gate = Lock()
        self._sinks: dict[str, LocalSink] = {}
        self._conn_targets: dict[str, set[Target]] = {}
        self._target_conns: dict[Target, set[str]] = {}

    async def attach(self, conn_id: str, sink: LocalSink) -> None:
        with self._gate:
            self._sinks[conn_id] = sink
            self._link(conn_id, Target("connection", conn_id))

    async def detach(self, conn_id: str) -> None:
        with self._gate:
            self._sinks.pop(conn_id, None)
            for target in self._conn_targets.pop(conn_id, set()):
                conns = self._target_conns.get(target)
                if conns is None:
                    continue
                conns.discard(conn_id)
                if not conns:
                    del self._target_conns[target]

    async def associate(self, conn_id: str, target: Target) -> None:
        with self._gate:
            self._link(conn_id, target)

    async def publish(self, target: Target, event: dict[str, Any]) -> int:
        with self._gate:
            # Snapshot the sinks under the lock, then deliver outside it. The sinks
            # are non-blocking enqueues, but a host's sink is arbitrary code and
            # calling it under the registry lock would let it deadlock every
            # connection.
            sinks = [self._sinks[c] for c in self._target_conns.get(target, set()) if c in self._sinks]
        for sink in sinks:
            sink(event)
        return len(sinks)

    def _link(self, conn_id: str, target: Target) -> None:
        """Record both directions of a conn↔target association. Caller holds the gate."""
        self._conn_targets.setdefault(conn_id, set()).add(target)
        self._target_conns.setdefault(target, set()).add(conn_id)

    @property
    def attached_count(self) -> int:
        """How many connections are currently attached (for tests/diagnostics)."""
        with self._gate:
            return len(self._sinks)
