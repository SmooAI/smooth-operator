"""Unit tests for the ``identity_intake`` Rich Interaction kind's validator + parser.

Mirrors the Rust reference tests in ``rust/smooth-operator/src/identity_intake.rs``, and
additionally validates the shared conformance fixtures
(``spec/conformance/fixtures.json``) so the Python validator agrees with the golden spec
every language checks against.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from smooth_operator_server.identity_intake import (
    IdentityIntakeKind,
    normalize_email,
    normalize_phone_e164,
    parse_fields,
    validate_intake,
)

_SPEC = Path(__file__).resolve().parents[3] / "spec" / "conformance" / "fixtures.json"


@pytest.fixture(scope="module")
def fixtures() -> dict:
    return json.loads(_SPEC.read_text())


def _field(key: str, required: bool) -> dict:
    return {"key": key, "required": required}


def test_email_shapes() -> None:
    assert normalize_email("Alice@Example.COM") == "Alice@example.com", "domain lowercased, local preserved"
    for bad in ["", "no-at", "@x.com", "a@b", "a@.com", "a@b.", "a b@c.com", "a@b@c.com"]:
        assert normalize_email(bad) is None, f"{bad!r} should be rejected"


def test_phone_shapes() -> None:
    assert normalize_phone_e164("+1 (555) 123-4567") == "+15551234567"
    assert normalize_phone_e164("555.123.4567") == "+15551234567", "bare 10-digit NANP"
    assert normalize_phone_e164("1 555 123 4567") == "+15551234567", "1-prefixed 11-digit NANP"
    assert normalize_phone_e164("+447911123456") == "+447911123456", "non-NANP with country code"
    for bad in ["", "abc", "+0123456789", "12345", "+1234567890123456"]:
        assert normalize_phone_e164(bad) is None, f"{bad!r} should be rejected"


def test_required_field_missing_is_an_error() -> None:
    fields = [_field("email", True), _field("name", False)]
    canonical, errors = validate_intake(fields, {})
    assert canonical is None
    assert len(errors) == 1
    assert errors[0].field == "email"

    # Blank counts as missing.
    canonical2, errors2 = validate_intake(fields, {"email": "   "})
    assert canonical2 is None
    assert any(e.field == "email" for e in errors2)


def test_optional_field_absent_is_fine() -> None:
    # name optional and absent, email optional and present → valid, only email kept.
    canonical, errors = validate_intake([_field("name", False), _field("email", False)], {"email": "a@b.co"})
    assert errors == []
    assert canonical == {"email": "a@b.co"}


def test_valid_submit_normalizes() -> None:
    fields = [_field("email", True), _field("phone", False)]
    values = {"name": "  Alice Example  ", "email": "alice@Example.com", "phone": "(555) 123-4567"}
    canonical, errors = validate_intake(fields, values)
    assert errors == []
    assert canonical == {"name": "Alice Example", "email": "alice@example.com", "phone": "+15551234567"}


def test_bad_email_is_a_field_error() -> None:
    canonical, errors = validate_intake([_field("email", True)], {"email": "not-an-email"})
    assert canonical is None
    assert len(errors) == 1
    assert errors[0].field == "email"
    assert "valid email" in errors[0].message


def test_all_errors_reported_in_one_pass() -> None:
    # missing required name + bad email + bad phone → three field errors, one round-trip.
    canonical, errors = validate_intake([_field("name", True)], {"email": "not-an-email", "phone": "nope"})
    assert canonical is None
    assert len(errors) == 3, f"{errors!r}"
    assert {e.field for e in errors} == {"name", "email", "phone"}


def test_volunteered_field_is_kept() -> None:
    # Only email requested, but the visitor volunteered a phone — keep it.
    canonical, errors = validate_intake([_field("email", True)], {"email": "a@b.co", "phone": "+15551234567"})
    assert errors == []
    assert canonical["phone"] == "+15551234567"


def test_parse_fields_accepts_structured_and_shorthand() -> None:
    # Structured form.
    fields = parse_fields([{"key": "email", "required": True, "label": "Work email"}])
    assert fields == [{"key": "email", "required": True, "label": "Work email"}]
    # Shorthand strings → required: True.
    fields2 = parse_fields(["name", "phone"])
    assert fields2 == [{"key": "name", "required": True}, {"key": "phone", "required": True}]
    # required defaults to True when omitted in the object form.
    assert parse_fields([{"key": "phone"}]) == [{"key": "phone", "required": True}]


def test_parse_fields_enforces_the_contract() -> None:
    with pytest.raises(ValueError):  # not an array
        parse_fields("email")
    with pytest.raises(ValueError):  # empty
        parse_fields([])
    with pytest.raises(ValueError):  # unknown key
        parse_fields(["ssn"])
    with pytest.raises(ValueError):  # object without a string key
        parse_fields([{"required": True}])


def test_kind_wires_the_reference_surface() -> None:
    kind = IdentityIntakeKind()
    assert kind.kind() == "identity_intake"
    assert kind.capability() == "identity_form"
    assert kind.tool_schema()["name"] == "request_identity_intake"

    req = kind.parse_request({"fields": ["email", {"key": "phone", "required": False}], "reason": "to send the quote"})
    assert req.kind == "identity_intake"
    assert req.reason == "to send the quote"
    assert req.spec["fields"][0] == {"key": "email", "required": True}

    canonical, errors = kind.validate(req.spec, {"email": "a@b.co"})
    assert errors == []
    assert canonical == {"email": "a@b.co"}

    # Empty submit (no field carries a value) is rejected with a values-level error.
    none_canonical, none_errors = kind.validate(req.spec, {})
    assert none_canonical is None
    assert none_errors[0].field == "values"

    directive = kind.fallback_directive(req.spec, "to send the quote")
    assert "email, phone" in directive
    assert "submit_interaction" in directive


def test_shared_fixtures_validate_to_the_canonical_payload(fixtures: dict) -> None:
    """The shared ``identity_intake`` fixtures: the golden spec + values must validate to
    exactly the golden payload's values (the same golden shapes the Rust/Go/TS/C# engines
    check)."""
    spec = fixtures["identity_intake_spec"]["instance"]
    values = fixtures["identity_intake_values"]["instance"]
    expected = fixtures["identity_intake_payload"]["instance"]["values"]

    canonical, errors = IdentityIntakeKind().validate(spec, values)
    assert errors == []
    assert canonical == expected
