"""OTP protocol conformance — the server's OTP event builders reproduce the shared
golden fixtures (``spec/conformance/fixtures.json``) byte-for-byte.

The OTP fixtures were added to the shared spec by the Rust PR (#132); this asserts
the Python builders emit the same wire shapes every language validates against.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from smooth_operator_server import protocol

_SPEC = Path(__file__).resolve().parents[3] / "spec" / "conformance" / "fixtures.json"


@pytest.fixture(scope="module")
def fixtures() -> dict:
    return json.loads(_SPEC.read_text())


def _strip_timestamp(event: dict) -> dict:
    return {k: v for k, v in event.items() if k != "timestamp"}


def test_otp_verification_required_matches_fixture(fixtures: dict) -> None:
    fixture = fixtures["otp_verification_required_event"]["instance"]
    inner = fixture["data"]["data"]
    built = protocol.otp_verification_required(
        fixture["requestId"],
        inner["toolId"],
        inner["actionDescription"],
        inner["availableChannels"],
        inner["authLevel"],
    )
    assert _strip_timestamp(built) == _strip_timestamp(fixture)


def test_otp_sent_matches_fixture(fixtures: dict) -> None:
    fixture = fixtures["otp_sent_event"]["instance"]
    inner = fixture["data"]["data"]
    built = protocol.otp_sent(fixture["requestId"], inner["channel"], inner["maskedDestination"])
    assert _strip_timestamp(built) == _strip_timestamp(fixture)


def test_otp_verified_matches_fixture(fixtures: dict) -> None:
    fixture = fixtures["otp_verified_event"]["instance"]
    inner = fixture["data"]["data"]
    built = protocol.otp_verified(fixture["requestId"], inner["message"])
    assert _strip_timestamp(built) == _strip_timestamp(fixture)


def test_otp_invalid_matches_fixture(fixtures: dict) -> None:
    fixture = fixtures["otp_invalid_event"]["instance"]
    inner = fixture["data"]["data"]
    built = protocol.otp_invalid(
        fixture["requestId"],
        inner.get("error"),
        inner["attemptsRemaining"],
        inner["message"],
    )
    assert _strip_timestamp(built) == _strip_timestamp(fixture)


def test_otp_invalid_omits_error_when_none() -> None:
    """`error` is optional per the schema — absent (not null) when the host couldn't
    determine a cause."""
    ev = protocol.otp_invalid("r1", None, 0, "Verification failed.")
    assert "error" not in ev["data"]["data"]
    assert ev["data"]["data"]["attemptsRemaining"] == 0


def test_otp_verify_request_fixture_round_trips(fixtures: dict) -> None:
    """The client-side `verify_otp` request fixture survives a JSON encode/decode
    (the shape the dispatcher parses)."""
    instance = fixtures["verify_otp_request"]["instance"]
    assert json.loads(json.dumps(instance)) == instance
    assert instance["action"] == "verify_otp"
    assert set(instance) >= {"action", "sessionId", "requestId", "code"}
