"""Write-confirmation HITL — the pending-confirmation registry.

When an agent turn calls a tool that requires human approval, the turn **parks**
inside the engine's :class:`~smooth_operator_core.HumanGate` and the runner
registers a resolver here, keyed by ``sessionId``. A subsequent
``confirm_tool_action`` frame on the same connection looks the session up, resolves
the future with the verdict, and the parked turn resumes (runs the tool on approve;
skips it with a rejection result on deny).

The Python analog of the Rust ``AppState`` pending-confirmation map
(``register_confirmation`` / ``take_confirmation`` / ``clear_confirmation``) and the
C# pending-confirmation registry. Keyed by session so each session has at most one
outstanding confirmation; an empty registry means no turn is parked (the default,
behavior identical to before HITL).
"""

from __future__ import annotations

import asyncio


class ConfirmationRegistry:
    """Tracks the in-flight write-confirmation each parked turn is waiting on.

    Single-threaded under the asyncio event loop: every method runs on the loop, so
    the dict needs no extra locking (unlike the cross-thread session store)."""

    def __init__(self) -> None:
        #: ``sessionId`` → the future a parked turn awaits. ``True`` = approved,
        #: ``False`` = rejected. At most one per session.
        self._pending: dict[str, asyncio.Future[bool]] = {}

    def register(self, session_id: str) -> asyncio.Future[bool]:
        """Register (and return) a fresh approval future for ``session_id``.

        Any prior pending future for the session is rejected (resolved ``False``)
        first, so a stale parked turn can never be left dangling and the newest
        confirmation always wins — mirrors the Rust ``register_confirmation`` taking
        over a prior sender."""
        prior = self._pending.pop(session_id, None)
        if prior is not None and not prior.done():
            prior.set_result(False)
        future: asyncio.Future[bool] = asyncio.get_running_loop().create_future()
        self._pending[session_id] = future
        return future

    def resolve(self, session_id: str, approved: bool) -> bool:
        """Resolve the parked turn for ``session_id`` with the verdict.

        Returns ``True`` if a pending confirmation was resolved, ``False`` if none
        was awaiting (a duplicate/stale ``confirm_tool_action`` → ``NO_PENDING_CONFIRMATION``).
        Taking the future out makes a duplicate confirm a clean no-op (mirrors the
        Rust ``take_confirmation``)."""
        future = self._pending.pop(session_id, None)
        if future is None or future.done():
            return False
        future.set_result(approved)
        return True

    def clear(self, session_id: str) -> None:
        """Drop any registered future for ``session_id`` (turn ended), so a stale
        entry can't mis-route a later confirmation. Idempotent."""
        self._pending.pop(session_id, None)

    def reject_all(self) -> None:
        """Resolve every outstanding confirmation as **rejected** (deny).

        Called when a connection is torn down (close / graceful drain) so any turn
        parked on a confirmation unparks and finishes cleanly — fail closed (a write
        is never auto-approved on disconnect) and never leave a turn hung forever."""
        for future in tuple(self._pending.values()):
            if not future.done():
                future.set_result(False)
        self._pending.clear()
