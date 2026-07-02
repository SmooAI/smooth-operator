"""End-to-end OTP identity-verification flow — the ``verify_otp`` action + the
post-turn OTP offer, driven through the real :class:`FrameDispatcher`.

Mirrors the Rust ``otp_flow.rs`` (verify happy/invalid/no-service/unknown-session/
missing-field) and the ``agent_tool_auth.rs`` OTP-offer additions (end_user refusal
offers OTP, admin refusal does not), plus the Python-specific fail-closed edges
(no contact, no service, send failure, verified session runs the gated tool).

A stub host :class:`OtpService` stands in for the credential owner — the reference
server never generates or validates a code itself.
"""

from __future__ import annotations

import json

import pytest
from smooth_operator_core import MockLlmProvider

from smooth_operator_server.agent_config import AgentConfig, EnabledTool, StaticAgentConfigResolver
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.otp import (
    OtpChannel,
    OtpContact,
    OtpDelivery,
    OtpError,
    OtpInvalid,
    OtpService,
    OtpVerified,
    OtpVerifyOutcome,
)
from smooth_operator_server.session_store import InMemorySessionStore


class StubOtp(OtpService):
    """Fixed-outcome host stub. ``send_otp`` "delivers" to a masked email and records
    its calls; ``verify_otp`` returns the configured outcome."""

    def __init__(self, outcome: OtpVerifyOutcome | None = None) -> None:
        self.outcome = outcome or OtpVerified()
        self.sent: list[tuple[str, OtpContact]] = []

    async def send_otp(self, session_id: str, contact: OtpContact) -> OtpDelivery:
        self.sent.append((session_id, contact))
        return OtpDelivery(channel=OtpChannel.EMAIL, masked_destination="j***@example.com")

    async def verify_otp(self, session_id: str, code: str) -> OtpVerifyOutcome:
        return self.outcome


class FailingSendOtp(StubOtp):
    """A host whose delivery channel is down — ``send_otp`` raises."""

    async def send_otp(self, session_id: str, contact: OtpContact) -> OtpDelivery:
        raise RuntimeError("email provider unavailable")


def _types(events: list[dict]) -> list[str]:
    return [e.get("type") for e in events]


async def _dispatch(dispatcher: FrameDispatcher, frame: dict) -> list[dict]:
    """Dispatch one frame, collecting every event emitted to the sink."""
    events: list[dict] = []
    await dispatcher.dispatch(json.dumps(frame), events.append)
    return events


# --------------------------------------------------------------------------- #
# verify_otp action
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_verify_otp_success_marks_session_authenticated() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent-otp", "Alice", "alice@example.com")
    dispatcher = FrameDispatcher(store, None, otp_service=StubOtp(OtpVerified()))

    assert await store.is_session_authenticated(session.session_id) is False

    events = await _dispatch(
        dispatcher,
        {"action": "verify_otp", "requestId": "vo-1", "sessionId": session.session_id, "code": "123456"},
    )
    assert _types(events) == ["otp_verified"]
    assert events[0]["requestId"] == "vo-1"
    assert events[0]["data"]["data"]["message"] == "Identity verified successfully."
    assert await store.is_session_authenticated(session.session_id) is True


@pytest.mark.asyncio
async def test_verify_otp_invalid_reflects_host_attempts_and_reason() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent-otp", "Alice", "alice@example.com")
    outcome = OtpInvalid(
        attempts_remaining=2, message="Invalid code. 2 attempt(s) remaining.", error=OtpError.INVALID_CODE
    )
    dispatcher = FrameDispatcher(store, None, otp_service=StubOtp(outcome))

    events = await _dispatch(
        dispatcher,
        {"action": "verify_otp", "requestId": "vo-1", "sessionId": session.session_id, "code": "000000"},
    )
    assert _types(events) == ["otp_invalid"]
    inner = events[0]["data"]["data"]
    assert inner["attemptsRemaining"] == 2
    assert inner["error"] == "INVALID_CODE"
    # A rejected code must NOT authenticate the session.
    assert await store.is_session_authenticated(session.session_id) is False


@pytest.mark.asyncio
async def test_verify_otp_invalid_without_error_omits_error_key() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent-otp", None, "a@example.com")
    outcome = OtpInvalid(attempts_remaining=0, message="Verification failed.")  # no machine reason
    dispatcher = FrameDispatcher(store, None, otp_service=StubOtp(outcome))

    events = await _dispatch(
        dispatcher,
        {"action": "verify_otp", "requestId": "vo-1", "sessionId": session.session_id, "code": "x"},
    )
    assert "error" not in events[0]["data"]["data"]
    assert events[0]["data"]["data"]["attemptsRemaining"] == 0


@pytest.mark.asyncio
async def test_verify_otp_without_service_fails_closed() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent-otp", None, "a@example.com")
    dispatcher = FrameDispatcher(store, None)  # no otp_service

    events = await _dispatch(
        dispatcher,
        {"action": "verify_otp", "requestId": "vo-1", "sessionId": session.session_id, "code": "123456"},
    )
    assert _types(events) == ["otp_invalid"]
    inner = events[0]["data"]["data"]
    assert inner["error"] == "NOT_FOUND"
    assert inner["attemptsRemaining"] == 0
    assert await store.is_session_authenticated(session.session_id) is False


@pytest.mark.asyncio
async def test_verify_otp_unknown_session_errors() -> None:
    # Adversarial: a code for a session the server doesn't track authenticates nothing.
    store = InMemorySessionStore()
    dispatcher = FrameDispatcher(store, None, otp_service=StubOtp(OtpVerified()))
    events = await _dispatch(
        dispatcher,
        {"action": "verify_otp", "requestId": "vo-1", "sessionId": "no-such-session", "code": "123456"},
    )
    assert events[0]["type"] == "error"
    assert events[0]["error"]["code"] == "SESSION_NOT_FOUND"


@pytest.mark.asyncio
async def test_verify_otp_missing_request_id_is_validation_error() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent-otp", None, "a@example.com")
    dispatcher = FrameDispatcher(store, None, otp_service=StubOtp())
    events = await _dispatch(
        dispatcher,
        {"action": "verify_otp", "sessionId": session.session_id, "code": "123456"},
    )
    assert events[0]["type"] == "error"
    assert events[0]["error"]["code"] == "VALIDATION_ERROR"


@pytest.mark.asyncio
async def test_verify_otp_missing_session_id_is_validation_error() -> None:
    store = InMemorySessionStore()
    dispatcher = FrameDispatcher(store, None, otp_service=StubOtp())
    events = await _dispatch(dispatcher, {"action": "verify_otp", "requestId": "vo-1", "code": "123456"})
    assert events[0]["type"] == "error"
    assert events[0]["error"]["code"] == "VALIDATION_ERROR"


@pytest.mark.asyncio
async def test_verify_otp_missing_code_is_validation_error() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent-otp", None, "a@example.com")
    dispatcher = FrameDispatcher(store, None, otp_service=StubOtp())
    events = await _dispatch(
        dispatcher,
        {"action": "verify_otp", "requestId": "vo-1", "sessionId": session.session_id},
    )
    assert events[0]["type"] == "error"
    assert events[0]["error"]["code"] == "VALIDATION_ERROR"


# --------------------------------------------------------------------------- #
# Post-turn OTP offer (send_message path)
# --------------------------------------------------------------------------- #


class SpyTool:
    """Duck-typed engine Tool that opts into auth gating and records invocations."""

    def __init__(self, name: str, *, supports_auth: bool = True) -> None:
        self.name = name
        self.description = f"the {name} tool"
        self.parameters = {"type": "object", "properties": {}}
        self.supports_auth_requirement = supports_auth
        self.calls: list[dict] = []

    async def execute(self, arguments: dict) -> str:
        self.calls.append(arguments)
        return f"ran {self.name}"


async def _run_gated_turn(
    *,
    auth_level: str,
    visibility: str,
    user_email: str | None,
    otp_service: OtpService | None,
    pre_authenticated: bool = False,
) -> tuple[SpyTool, list[dict], InMemorySessionStore, str]:
    """Open a session (optionally pre-verified), drive one send_message whose LLM
    calls a gated tool, and return (tool, events, store, session_id)."""
    store = InMemorySessionStore()
    session = await store.create_session("agent-x", None, user_email)
    if pre_authenticated:
        await store.set_session_authenticated(session.session_id, True)

    tool = SpyTool("pay_invoice")
    config = AgentConfig(enabled_tools=[EnabledTool("pay_invoice", auth_level=auth_level)], visibility=visibility)

    mock = MockLlmProvider()
    mock.push_tool_call("call-1", tool.name, "{}")  # LLM asks for the gated tool
    mock.push_text("all done")  # follow-up reply after the tool result

    dispatcher = FrameDispatcher(
        store,
        mock,
        tools=[tool],
        agent_config_resolver=StaticAgentConfigResolver({"agent-x": config}),
        otp_service=otp_service,
    )
    events: list[dict] = []
    await dispatcher.dispatch(
        json.dumps({"action": "send_message", "sessionId": session.session_id, "message": "pay it"}),
        events.append,
    )
    await dispatcher.wait_for_turns()
    return tool, events, store, session.session_id


@pytest.mark.asyncio
async def test_end_user_refusal_offers_otp_in_order() -> None:
    otp = StubOtp()
    tool, events, _store, session_id = await _run_gated_turn(
        auth_level="end_user", visibility="public", user_email="alice@example.com", otp_service=otp
    )
    assert tool.calls == []  # gated tool never ran
    types = _types(events)
    # The offer sequence must appear, in order, before the terminal eventual_response.
    i_req = types.index("otp_verification_required")
    i_sent = types.index("otp_sent")
    i_resp = types.index("eventual_response")
    assert i_req < i_sent < i_resp
    # The prompt names the tool + offers the email channel the contact supports.
    prompt = events[i_req]["data"]["data"]
    assert prompt["toolId"] == "pay_invoice"
    assert prompt["authLevel"] == "end_user"
    assert prompt["availableChannels"] == ["email"]
    assert events[i_sent]["data"]["data"] == {"channel": "email", "maskedDestination": "j***@example.com"}
    # The host was asked to deliver to the session's contact.
    assert otp.sent == [(session_id, OtpContact(email="alice@example.com"))]


@pytest.mark.asyncio
async def test_admin_refusal_does_not_offer_otp() -> None:
    otp = StubOtp()
    tool, events, _store, _sid = await _run_gated_turn(
        auth_level="admin", visibility="public", user_email="alice@example.com", otp_service=otp
    )
    assert tool.calls == []
    assert "otp_verification_required" not in _types(events)
    assert otp.sent == []  # never asked to deliver a code


@pytest.mark.asyncio
async def test_no_contact_does_not_offer_otp() -> None:
    # A refusal with no reachable contact can't offer OTP (no channel to deliver to).
    otp = StubOtp()
    _tool, events, _store, _sid = await _run_gated_turn(
        auth_level="end_user", visibility="public", user_email=None, otp_service=otp
    )
    assert "otp_verification_required" not in _types(events)
    assert otp.sent == []


@pytest.mark.asyncio
async def test_no_service_does_not_offer_otp() -> None:
    _tool, events, _store, _sid = await _run_gated_turn(
        auth_level="end_user", visibility="public", user_email="alice@example.com", otp_service=None
    )
    assert "otp_verification_required" not in _types(events)


@pytest.mark.asyncio
async def test_verified_session_runs_gated_tool_and_offers_nothing() -> None:
    # A session already OTP-verified passes the end_user gate → the tool runs and no
    # OTP is offered (the parity end state: verify once, then the gated tool works).
    otp = StubOtp()
    tool, events, _store, _sid = await _run_gated_turn(
        auth_level="end_user",
        visibility="public",
        user_email="alice@example.com",
        otp_service=otp,
        pre_authenticated=True,
    )
    assert tool.calls == [{}]  # executed
    assert "otp_verification_required" not in _types(events)
    assert otp.sent == []


@pytest.mark.asyncio
async def test_send_otp_failure_emits_error_event() -> None:
    # Delivery failed → the client is told, and no otp_sent ack is emitted.
    _tool, events, _store, _sid = await _run_gated_turn(
        auth_level="end_user", visibility="public", user_email="alice@example.com", otp_service=FailingSendOtp()
    )
    types = _types(events)
    assert "otp_verification_required" in types
    assert "otp_sent" not in types
    err = next(e for e in events if e["type"] == "error")
    assert err["error"]["code"] == "OTP_SEND_FAILED"
