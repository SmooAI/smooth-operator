"""Conformance: every instance in ``spec/conformance/fixtures.json`` must validate
against the schema it claims to (mirrors the spec's own validation, in Python via
``jsonschema`` Draft 2020-12)."""

from __future__ import annotations

import json

import pytest

from smooth_operator.validate import DEFAULT_SPEC_DIR, ProtocolValidator, format_errors

SPEC_DIR = DEFAULT_SPEC_DIR
FIXTURES = json.loads((SPEC_DIR / "conformance" / "fixtures.json").read_text())
# Drop the leading ``$comment`` key; keep only real fixtures.
FIXTURES = {k: v for k, v in FIXTURES.items() if not k.startswith("$")}


@pytest.fixture(scope="module")
def validator() -> ProtocolValidator:
    return ProtocolValidator.load(SPEC_DIR)


def test_exposes_the_five_documented_fixtures() -> None:
    assert {
        "create_session_request",
        "create_session_response",
        "send_message_request",
        "stream_chunk_event",
        "eventual_response_event",
    } <= set(FIXTURES)


@pytest.mark.parametrize("name", list(FIXTURES))
def test_every_fixture_validates_against_its_declared_schema(name: str, validator: ProtocolValidator) -> None:
    fixture = FIXTURES[name]
    result = validator.validate_at(fixture["$schema_ref"], fixture["instance"])
    assert result.valid, f"{name} ({fixture['$schema_ref']}): {format_errors(result.errors)}"


def test_rejects_a_fixture_mutated_to_violate_its_schema(
    validator: ProtocolValidator,
) -> None:
    fixture = FIXTURES["stream_chunk_event"]
    broken = json.loads(json.dumps(fixture["instance"]))
    broken["type"] = "not_a_real_event"
    result = validator.validate_at(fixture["$schema_ref"], broken)
    assert not result.valid
    assert result.errors


def test_validate_action_routes_send_message_to_its_schema(
    validator: ProtocolValidator,
) -> None:
    send = FIXTURES["send_message_request"]["instance"]
    assert validator.validate_action(send).valid


def test_validate_event_routes_stream_chunk_to_its_schema(
    validator: ProtocolValidator,
) -> None:
    chunk = FIXTURES["stream_chunk_event"]["instance"]
    assert validator.validate_event(chunk).valid


def test_validate_action_rejects_missing_required_field(
    validator: ProtocolValidator,
) -> None:
    # send_message requires `message`; omit it.
    result = validator.validate_action({"action": "send_message", "sessionId": "x"})
    assert not result.valid


def test_validate_event_rejects_unknown_type(validator: ProtocolValidator) -> None:
    result = validator.validate_event({"type": "bogus"})
    assert not result.valid
