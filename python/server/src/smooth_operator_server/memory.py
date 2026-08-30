"""Durable auto-recall — the server side of the engine's ``Memory`` seam (Rust PR #330).

The engine already knows how to auto-recall: give ``AgentOptions.memory`` a store and it
pulls the entries relevant to the user's message into the turn's context. What was missing
on this server is the way for a HOST to say *which* store — so every turn ran without
auto-recall regardless of what the deployment had.

Mirrors the Rust ``StorageAdapter::memory_for_access`` seam. ``access`` is threaded (as it is
for knowledge) so a multi-tenant backend can bind memory to the requester's org/user; a
single-tenant host — Big Smooth's daemon, the reason this seam exists — ignores it and
returns its one store.
"""

from __future__ import annotations

from typing import Protocol, runtime_checkable

from smooth_operator_core import Memory

from .auth import AccessContext


@runtime_checkable
class MemoryProvider(Protocol):
    """Supplies the durable-recall handle for a turn."""

    def memory_for_access(self, access: AccessContext) -> Memory | None:
        """The memory to auto-recall from for a caller with this access, or ``None`` for none.

        ``None`` is the default for every deployment that has not opted in, and leaves the
        turn byte-for-byte unchanged."""
        ...


class StaticMemoryProvider:
    """A :class:`MemoryProvider` over one unscoped store — the single-tenant case (Big Smooth's
    daemon hands its SQLite-backed store straight through). A multi-tenant host implements the
    protocol itself and keys off ``access`` instead."""

    def __init__(self, memory: Memory | None) -> None:
        self._memory = memory

    def memory_for_access(self, access: AccessContext) -> Memory | None:
        return self._memory
