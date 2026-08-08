"""An ``INTERNAL_ERROR`` on the wire must leave a traceback in the host log (th-e7ef23).

Both places the dispatcher maps an exception to ``INTERNAL_ERROR`` used to swallow it, so a
server failing every request logged nothing at all — the failure mode that hid th-df7007 in
the .NET sibling (a 401 from the gateway, invisible for a whole bench run). The wire message
stays generic; the detail goes to the log.
"""

from __future__ import annotations

import json
import logging

from smooth_operator_server.auth import AccessContext
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore


class _ExplodingStore(InMemorySessionStore):
    """A store whose ``get_session`` fails — the cheapest way into the error path."""

    async def get_session(self, session_id: str):  # type: ignore[override]
        raise RuntimeError("gateway said 401")


async def test_internal_error_is_logged_not_swallowed(caplog) -> None:
    dispatcher = FrameDispatcher(_ExplodingStore(), None, access=AccessContext.ANONYMOUS)
    events: list[dict] = []

    with caplog.at_level(logging.ERROR, logger="smooth_operator_server.dispatcher"):
        await dispatcher.dispatch(
            json.dumps({"action": "get_session", "requestId": "r-9", "sessionId": "s-1"}), events.append
        )

    # Wire contract unchanged: generic code + message, no exception detail leaked.
    assert events[-1]["error"]["code"] == "INTERNAL_ERROR"
    assert "gateway said 401" not in json.dumps(events[-1])

    # …but the host log has the real cause, at ERROR level, with the traceback attached.
    record = next(r for r in caplog.records if r.levelno == logging.ERROR)
    assert "get_session" in record.getMessage()
    assert "r-9" in record.getMessage()
    assert record.exc_info is not None
    assert "gateway said 401" in caplog.text
