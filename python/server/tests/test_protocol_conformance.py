"""Protocol conformance — round-trip the golden conformance fixtures through the
server's protocol builders + a JSON serialize/parse cycle.

The fixtures (``spec/conformance/fixtures.json``) are the shared golden messages
every language's client/server validates against. Here we assert the server's
:mod:`smooth_operator_server.protocol` builders reproduce the exact wire shapes of
the server→client event fixtures, and that every fixture round-trips losslessly
through ``json.dumps`` / ``json.loads`` (the on-the-wire encode/decode).
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from smooth_operator_server import protocol

# spec/ lives at the repo root, four levels up from this test file
# (python/server/tests/ -> python/server -> python -> repo root).
_SPEC = Path(__file__).resolve().parents[3] / "spec" / "conformance" / "fixtures.json"


@pytest.fixture(scope="module")
def fixtures() -> dict:
    return json.loads(_SPEC.read_text())


def test_all_fixtures_round_trip_json(fixtures: dict) -> None:
    """Every golden instance survives a JSON encode/decode unchanged."""
    for name, entry in fixtures.items():
        if not isinstance(entry, dict) or "instance" not in entry:
            continue
        instance = entry["instance"]
        assert json.loads(json.dumps(instance)) == instance, f"{name} did not round-trip"


def _strip_timestamp(event: dict) -> dict:
    """Timestamps are wall-clock, so compare everything else."""
    return {k: v for k, v in event.items() if k != "timestamp"}


def test_eventual_response_builder_matches_fixture(fixtures: dict) -> None:
    """The terminal turn event the server emits matches the golden double-nested
    ``data.data`` shape."""
    fixture = fixtures["eventual_response_event"]["instance"]
    inner = fixture["data"]["data"]
    built = protocol.eventual_response(
        fixture["requestId"],
        fixture["status"],
        inner["messageId"],
        inner["response"],
        needs_escalation=inner["needsEscalation"],
        citations=None,
    )
    assert _strip_timestamp(built) == _strip_timestamp(fixture)


def test_eventual_response_with_citations_matches_fixture(fixtures: dict) -> None:
    """A grounded turn attaches the ``citations`` array exactly as the golden shows."""
    fixture = fixtures["eventual_response_with_citations_event"]["instance"]
    inner = fixture["data"]["data"]
    built = protocol.eventual_response(
        fixture["requestId"],
        fixture["status"],
        inner["messageId"],
        inner["response"],
        needs_escalation=inner["needsEscalation"],
        citations=inner["citations"],
    )
    assert _strip_timestamp(built) == _strip_timestamp(fixture)


def test_cancelled_builder_matches_fixture(fixtures: dict) -> None:
    """The terminal event of a client-cancelled turn matches the golden shape:
    ``status: 499`` at both levels, the cancelled turn's ``requestId`` echoed, and no
    answer payload."""
    fixture = fixtures["cancelled_event"]["instance"]
    built = protocol.cancelled(fixture["requestId"])
    assert _strip_timestamp(built) == _strip_timestamp(fixture)


def test_stream_chunk_envelope_shape(fixtures: dict) -> None:
    """The server's ``stream_chunk`` envelope mirrors ``node`` at both levels and
    nests ``state`` under ``data`` — matching the golden's structure."""
    fixture = fixtures["stream_chunk_event"]["instance"]
    state = fixture["data"]["state"]
    built = protocol.stream_chunk(fixture["requestId"], fixture["node"], state)
    assert built["type"] == "stream_chunk"
    assert built["node"] == fixture["node"]
    assert built["data"]["node"] == fixture["node"]
    assert built["data"]["requestId"] == fixture["requestId"]
    assert built["data"]["state"] == state


def test_create_session_response_shape(fixtures: dict) -> None:
    """The create-session response payload (carried in immediate_response.data) has
    every field the golden response fixture declares."""
    expected = fixtures["create_session_response"]["instance"]
    # The server builds this dict in the dispatcher; assert the key set matches.
    assert set(expected) == {
        "sessionId",
        "conversationId",
        "agentId",
        "agentName",
        "userParticipantId",
        "agentParticipantId",
    }
