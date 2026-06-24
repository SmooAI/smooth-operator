"""Server→client protocol event builders.

The JSON shapes mirror the Rust reference server's ``protocol.rs`` (the canonical
implementation) byte-for-byte — including the triple-nested
``eventual_response.data.data`` payload and the duplicated ``error`` descriptor —
so they validate against the same ``spec/events/*.schema.json`` and the conformance
fixtures. This is the Python analog of the C# ``ProtocolEvents`` and the Rust
``protocol`` module.

Each builder returns a plain ``dict`` ready to be ``json.dumps``-ed onto the wire.
"""

from __future__ import annotations

import time
from typing import Any


def _now_ms() -> int:
    """Current Unix epoch milliseconds (the ``timestamp`` field on every event)."""
    return int(time.time() * 1000)


def pong(request_id: str | None) -> dict[str, Any]:
    """``pong`` — reply to a ``ping``. The timestamp is mirrored both at the
    envelope level and inside ``data`` (matching the Rust ref + ``pong.schema.json``)."""
    ts = _now_ms()
    ev: dict[str, Any] = {"type": "pong", "timestamp": ts, "data": {"timestamp": ts}}
    if request_id is not None:
        ev["requestId"] = request_id
    return ev


def immediate_response(request_id: str | None, status: int, message: str, data: Any) -> dict[str, Any]:
    """``immediate_response`` — synchronous ack. For non-streaming actions this
    also carries the full response payload in ``data``."""
    ev: dict[str, Any] = {
        "type": "immediate_response",
        "status": status,
        "message": message,
        "data": data,
        "timestamp": _now_ms(),
    }
    if request_id is not None:
        ev["requestId"] = request_id
    return ev


def stream_token(request_id: str, token: str) -> dict[str, Any]:
    """``stream_token`` — a single streamed text delta. The token is mirrored at
    the envelope level and inside ``data`` (per ``stream-token.schema.json``)."""
    return {
        "type": "stream_token",
        "requestId": request_id,
        "token": token,
        "data": {"requestId": request_id, "token": token},
        "timestamp": _now_ms(),
    }


def stream_chunk(request_id: str, node: str, state: Any) -> dict[str, Any]:
    """``stream_chunk`` — a per-node state snapshot (tool call / tool result).
    ``node`` is mirrored at the envelope level and inside ``data`` (per
    ``stream-chunk.schema.json``)."""
    return {
        "type": "stream_chunk",
        "requestId": request_id,
        "node": node,
        "data": {"requestId": request_id, "node": node, "state": state},
        "timestamp": _now_ms(),
    }


def eventual_response(
    request_id: str,
    status: int,
    message_id: str,
    response: Any,
    needs_escalation: bool,
    citations: list[dict[str, Any]] | None,
) -> dict[str, Any]:
    """``eventual_response`` — the terminal event of a streaming turn. The payload
    is double-nested (``data.data``) per ``eventual-response.schema.json``.

    ``citations`` are attached to the inner ``data.data.citations`` array ONLY when
    non-empty — absent otherwise, keeping the event back-compatible with clients
    that predate citations (matching the Rust ref)."""
    inner: dict[str, Any] = {
        "messageId": message_id,
        "response": response,
        "needsEscalation": needs_escalation,
    }
    if citations:
        inner["citations"] = citations
    return {
        "type": "eventual_response",
        "requestId": request_id,
        "status": status,
        "data": {"requestId": request_id, "status": status, "data": inner},
        "timestamp": _now_ms(),
    }


def error(request_id: str | None, code: str, message: str) -> dict[str, Any]:
    """``error`` — an unrecoverable error. The ``{code, message}`` descriptor is
    duplicated at the envelope level and nested under ``data.error`` for wire
    backward-compatibility (per ``error.schema.json``)."""
    descriptor = {"code": code, "message": message}
    data: dict[str, Any] = {"error": descriptor}
    if request_id is not None:
        data["requestId"] = request_id
    ev: dict[str, Any] = {
        "type": "error",
        "error": descriptor,
        "data": data,
        "timestamp": _now_ms(),
    }
    if request_id is not None:
        ev["requestId"] = request_id
    return ev


def general_agent_response(reply: str) -> dict[str, Any]:
    """A minimal ``GeneralAgentResponse`` wrapping the agent's reply text.

    The reference runner doesn't produce the full structured analytics, so it
    surfaces the reply in ``responseParts`` and supplies neutral defaults for the
    analytic fields (clients render ``responseParts``). Mirrors the Rust
    ``general_agent_response`` and the C# ``GeneralResponse``."""
    return {
        "responseParts": [reply],
        "customerHappinessScore": 0.5,
        "needsSatisfactionScore": 0.5,
        "requestSummary": "",
        "resolutionStatus": "in_progress",
        "suggestedNextActions": [],
    }


def citation(id: str, title: str, url: str | None, snippet: str, score: float) -> dict[str, Any]:
    """A single citation (the sources that grounded an answer). ``url`` is omitted
    (not null) for a source with no web location — matching the Rust/C# shape."""
    cite: dict[str, Any] = {"id": id, "title": title, "snippet": snippet, "score": score}
    if url is not None:
        cite["url"] = url
    return cite
