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


def stream_preamble(request_id: str, token: str) -> dict[str, Any]:
    """``stream_preamble`` — a token of the fast-model *preamble*: a short "what I'm
    about to do" sentence generated in parallel with the main turn to cover the
    reasoning model's time-to-first-token. Shaped exactly like ``stream_token`` so
    clients reuse the render path, but on a distinct ``type`` so it renders as an
    EPHEMERAL status line the real answer replaces — never folded into the answer,
    never persisted (per ``stream-preamble.schema.json``). Pearl th-ce3888."""
    return {
        "type": "stream_preamble",
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
    usage: dict[str, Any] | None = None,
    directive: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """``eventual_response`` — the terminal event of a streaming turn. The payload
    is double-nested (``data.data``) per ``eventual-response.schema.json``.

    ``citations`` are attached to the inner ``data.data.citations`` array ONLY when
    non-empty — absent otherwise, keeping the event back-compatible with clients
    that predate citations (matching the Rust ref).

    ``directive`` is an opaque client-side directive a host tool wrote onto the
    turn's directive sink (e.g. the ``send_file`` convention — agent→user file
    delivery). Attached to ``data.data.directive`` ONLY when non-``None`` — absent
    otherwise, keeping the event back-compatible with clients that predate
    directives (mirrors the Rust ref's ``protocol.rs`` directive handling).

    ``usage`` is the turn's token accounting + cost (``costUsd`` / ``promptTokens``
    / ``completionTokens``), captured from the engine's terminal ``DoneEvent`` so a
    client can accumulate live session cost. Attached to ``data.data.usage`` ONLY
    when non-``None`` — absent when the engine reported no usage, keeping the event
    back-compatible with clients that predate cost reporting (matching the Rust
    ref's ``usage: Option<TurnUsage>``)."""
    inner: dict[str, Any] = {
        "messageId": message_id,
        "response": response,
        "needsEscalation": needs_escalation,
    }
    if citations:
        inner["citations"] = citations
    if usage is not None:
        inner["usage"] = usage
    if directive is not None:
        inner["directive"] = directive
    return {
        "type": "eventual_response",
        "requestId": request_id,
        "status": status,
        "data": {"requestId": request_id, "status": status, "data": inner},
        "timestamp": _now_ms(),
    }


def write_confirmation_required(request_id: str, tool_id: str, action_description: str) -> dict[str, Any]:
    """``write_confirmation_required`` — emitted mid-turn when the agent calls a
    state-mutating tool that requires explicit human approval before it runs. The
    turn is **parked** (the engine's ``HumanGate`` awaits the verdict) until the
    client replies with a ``confirm_tool_action`` action carrying the same
    ``requestId`` and an ``approved`` boolean.

    Wire shape matches ``spec/events/write-confirmation-required.schema.json`` and
    the Rust reference's ``write_confirmation_required`` byte-for-byte: the
    ``requestId`` echoes the originating ``send_message``, and the prompt detail is
    double-nested under ``data.data.{toolId, actionDescription}``. ``toolId`` is an
    opaque correlation handle (the tool name — a turn parks one tool at a time);
    ``actionDescription`` is the human-readable prompt the client renders."""
    return {
        "type": "write_confirmation_required",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": {"toolId": tool_id, "actionDescription": action_description},
        },
        "timestamp": _now_ms(),
    }


def otp_verification_required(
    request_id: str,
    tool_id: str,
    action_description: str,
    available_channels: list[str],
    auth_level: str,
) -> dict[str, Any]:
    """``otp_verification_required`` — emitted after a turn's auth gate refused an
    ``end_user`` tool on an unverified session and a host OTP service is installed.
    Tells the client to collect a one-time code.

    Wire shape matches ``spec/events/otp-verification-required.schema.json`` and the
    Rust reference byte-for-byte (double-nested ``data.data``). ``available_channels``
    are the delivery channels the server can offer given the session's known contacts
    (``email`` / ``sms``); ``tool_id`` is the opaque tool handle awaiting verification;
    ``auth_level`` is the required level (fixed ``end_user`` on this flow)."""
    return {
        "type": "otp_verification_required",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": {
                "toolId": tool_id,
                "actionDescription": action_description,
                "availableChannels": available_channels,
                "authLevel": auth_level,
            },
        },
        "timestamp": _now_ms(),
    }


def otp_sent(request_id: str, channel: str, masked_destination: str) -> dict[str, Any]:
    """``otp_sent`` — acknowledgement that a code was dispatched to the caller. Wire
    shape matches ``spec/events/otp-sent.schema.json``. ``masked_destination`` is a
    partially masked address safe to display (e.g. ``j***@example.com``)."""
    return {
        "type": "otp_sent",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": {"channel": channel, "maskedDestination": masked_destination},
        },
        "timestamp": _now_ms(),
    }


def otp_verified(request_id: str, message: str) -> dict[str, Any]:
    """``otp_verified`` — emitted when a ``verify_otp`` attempt succeeds. The session
    is now identity-verified; the client re-sends its message to run the gated tool
    (the reference server does not park/auto-resume the original turn). Wire shape
    matches ``spec/events/otp-verified.schema.json``."""
    return {
        "type": "otp_verified",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": {"message": message},
        },
        "timestamp": _now_ms(),
    }


def otp_invalid(
    request_id: str,
    error: str | None,
    attempts_remaining: int,
    message: str,
) -> dict[str, Any]:
    """``otp_invalid`` — emitted when a ``verify_otp`` attempt is rejected. ``error``
    is an optional machine-readable reason (``INVALID_CODE`` / ``MAX_ATTEMPTS`` /
    ``NOT_FOUND`` / ``EXPIRED``); ``attempts_remaining`` of 0 means the code is locked
    and the client must restart the flow. Wire shape matches
    ``spec/events/otp-invalid.schema.json`` — ``error`` is omitted (not null) when the
    host couldn't determine a cause."""
    inner: dict[str, Any] = {"attemptsRemaining": attempts_remaining, "message": message}
    if error is not None:
        inner["error"] = error
    return {
        "type": "otp_invalid",
        "requestId": request_id,
        "data": {
            "requestId": request_id,
            "data": inner,
        },
        "timestamp": _now_ms(),
    }


def cancelled(request_id: str | None) -> dict[str, Any]:
    """``cancelled`` — the terminal event of a turn the client aborted with a
    ``cancel`` action. Emitted **in place of** the ``eventual_response`` a completed
    turn would send: it echoes the cancelled ``send_message``'s ``requestId`` so the
    client can correlate it to the in-flight turn and reset its UI (drop the streaming
    indicator, re-enable input).

    Status ``499`` mirrors nginx's "client closed request" — a terminal, non-200
    outcome distinct from a server error. The ``requestId`` is echoed at the envelope
    level and inside ``data`` (envelope convention). No answer payload: a cancelled
    turn produced no assistant message (the streamed tokens were ephemeral and are NOT
    persisted; the user's message stays persisted).

    A cancel with no active turn is a no-op and emits nothing — this builder is only
    called when a live turn was actually aborted. Mirrors the Rust ``protocol::cancelled``."""
    ts = _now_ms()
    data: dict[str, Any] = {"status": 499}
    if request_id is not None:
        data["requestId"] = request_id
    ev: dict[str, Any] = {
        "type": "cancelled",
        "status": 499,
        "data": data,
        "timestamp": ts,
    }
    if request_id is not None:
        ev["requestId"] = request_id
    return ev


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
