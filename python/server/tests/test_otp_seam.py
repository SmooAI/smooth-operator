"""OTP seam value types — channels, errors, contact channel derivation, outcomes.

Mirrors the Rust ``otp.rs`` unit tests: wire strings for the channel/error enums and
the email-first-then-SMS channel derivation from a contact.
"""

from __future__ import annotations

from smooth_operator_server.otp import (
    OtpChannel,
    OtpContact,
    OtpError,
    OtpInvalid,
    OtpVerified,
)


def test_channel_wire_strings() -> None:
    assert OtpChannel.EMAIL.value == "email"
    assert OtpChannel.SMS.value == "sms"


def test_error_wire_strings() -> None:
    assert OtpError.INVALID_CODE.value == "INVALID_CODE"
    assert OtpError.MAX_ATTEMPTS.value == "MAX_ATTEMPTS"
    assert OtpError.NOT_FOUND.value == "NOT_FOUND"
    assert OtpError.EXPIRED.value == "EXPIRED"


def test_empty_contact_offers_no_channels() -> None:
    contact = OtpContact()
    assert contact.is_empty
    assert contact.available_channels() == []


def test_email_only_contact_offers_email() -> None:
    contact = OtpContact(email="a@example.com")
    assert not contact.is_empty
    assert contact.available_channels() == [OtpChannel.EMAIL]


def test_phone_only_contact_offers_sms() -> None:
    contact = OtpContact(phone="+15551234567")
    assert not contact.is_empty
    assert contact.available_channels() == [OtpChannel.SMS]


def test_both_contacts_offer_email_then_sms() -> None:
    contact = OtpContact(email="a@example.com", phone="+15551234567")
    assert contact.available_channels() == [OtpChannel.EMAIL, OtpChannel.SMS]


def test_outcomes_are_distinct_types() -> None:
    """The verify outcome is a union the dispatcher branches on by type."""
    assert isinstance(OtpVerified(), OtpVerified)
    invalid = OtpInvalid(attempts_remaining=2, message="nope", error=OtpError.INVALID_CODE)
    assert isinstance(invalid, OtpInvalid)
    assert invalid.attempts_remaining == 2
    assert invalid.error is OtpError.INVALID_CODE
    # error is optional (host couldn't determine a cause).
    assert OtpInvalid(attempts_remaining=0, message="locked").error is None
