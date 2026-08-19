"""Rich Interaction + ephemeral-stream frames must survive the client's own dispatch.

``test_conformance.py`` already validates the interaction fixtures against the
schemas, but it never feeds them to :meth:`SmoothAgentClient._handle_frame` — which
is exactly the blind spot that let ``interaction_required`` / ``interaction_invalid``
/ ``stream_preamble`` / ``stream_reasoning`` be dropped at runtime. These tests push
the real fixtures through the real dispatch path.
"""

from __future__ import annotations

import asyncio
import json

import pytest
from test_client import make_client

from smooth_operator import EventType, is_server_event, parse_event
from smooth_operator.validate import DEFAULT_SPEC_DIR, ProtocolValidator, format_errors

SPEC_DIR = DEFAULT_SPEC_DIR
FIXTURES = json.loads((SPEC_DIR / "conformance" / "fixtures.json").read_text())
SUBMIT_REF = "actions/submit-interaction.schema.json#/$defs/Request"


@pytest.fixture(scope="module")
def validator() -> ProtocolValidator:
    return ProtocolValidator.load(SPEC_DIR)


def spec_event_discriminators() -> set[str]:
    """The `type` const of every schema in spec/events/ — the source of truth."""
    out: set[str] = set()
    for path in sorted((SPEC_DIR / "events").glob("*.schema.json")):
        schema = json.loads(path.read_text())
        const = schema.get("properties", {}).get("type", {}).get("const")
        if const:
            out.add(const)
    return out


def retarget(instance: dict, request_id: str) -> dict:
    """Rewrite a fixture's requestId (at every nesting depth) to correlate with the
    turn under test."""
    node = json.loads(json.dumps(instance))  # deep copy

    def walk(v: object) -> None:
        if isinstance(v, dict):
            if "requestId" in v:
                v["requestId"] = request_id
            for child in v.values():
                walk(child)

    walk(node)
    return node


def _terminal(request_id: str) -> dict:
    """A minimal, valid terminal eventual_response so the turn's iterator completes."""
    return {
        "type": "eventual_response",
        "requestId": request_id,
        "status": 200,
        "data": {
            "requestId": request_id,
            "status": 200,
            "data": {
                "messageId": "66666666-6666-6666-6666-666666666666",
                "response": {"responseParts": ["done"]},
                "needsEscalation": False,
            },
        },
    }


# ───────────────────────────── drift guard ────────────────────────────────────
def test_event_types_cover_spec() -> None:
    """Derives the expected set from spec/events/*.schema.json rather than from a
    list maintained here — a guard asserting against its own hand-written constant
    would lock the drift in instead of catching it. Adding an event schema without
    wiring it into EventType fails here, not silently at runtime."""
    spec_events = spec_event_discriminators()
    assert spec_events, "no event schemas discovered in spec/events"
    known = {e.value for e in EventType}
    missing = spec_events - known
    assert not missing, (
        f"spec/events declares {sorted(missing)} but EventType omits them: "
        "is_server_event rejects the frame and the dispatch loop drops it silently"
    )


def test_action_types_cover_spec() -> None:
    """Same guard for the client→server direction."""
    spec_actions: set[str] = set()
    for path in sorted((SPEC_DIR / "actions").glob("*.schema.json")):
        schema = json.loads(path.read_text())
        req = schema.get("$defs", {}).get("Request", schema)
        const = req.get("properties", {}).get("action", {}).get("const")
        if const:
            spec_actions.add(const)
    assert spec_actions, "no action schemas discovered in spec/actions"
    from smooth_operator import ActionType

    missing = spec_actions - {a.value for a in ActionType}
    assert not missing, f"spec/actions declares {sorted(missing)} but ActionType omits them"


def test_validator_schema_maps_cover_spec() -> None:
    """``validate.py`` keeps its OWN type→schema-file maps, untyped and drifting
    independently of ``EventType`` — they were stale in exactly the same way. Assert
    them against the spec directory too, not against the enums."""
    from smooth_operator.validate import _ACTION_SCHEMA_REF, _EVENT_SCHEMA_FILE

    missing_events = spec_event_discriminators() - set(_EVENT_SCHEMA_FILE)
    assert not missing_events, (
        f"spec/events declares {sorted(missing_events)} but _EVENT_SCHEMA_FILE omits them: "
        "validate_event() reports 'Unknown event type' for a frame the spec defines"
    )

    spec_actions: set[str] = set()
    for path in sorted((SPEC_DIR / "actions").glob("*.schema.json")):
        schema = json.loads(path.read_text())
        req = schema.get("$defs", {}).get("Request", schema)
        const = req.get("properties", {}).get("action", {}).get("const")
        if const:
            spec_actions.add(const)
    missing_actions = spec_actions - set(_ACTION_SCHEMA_REF)
    assert not missing_actions, f"spec/actions declares {sorted(missing_actions)} but _ACTION_SCHEMA_REF omits them"


# ───────────────────────── fixtures through dispatch ──────────────────────────
@pytest.mark.parametrize(
    ("fixture_name", "expected_type"),
    [
        ("interaction_required_event", "interaction_required"),
        ("interaction_invalid_event", "interaction_invalid"),
    ],
)
def test_interaction_fixtures_parse_not_dropped(fixture_name: str, expected_type: str) -> None:
    """The guard + parser must both accept the canonical fixture. Before the fix
    ``is_server_event`` returned False here and ``_handle_frame`` returned early."""
    instance = FIXTURES[fixture_name]["instance"]
    assert is_server_event(instance), f"{fixture_name} rejected by is_server_event — frame would be dropped"
    event = parse_event(instance)
    assert event.type == expected_type


async def test_interaction_required_and_invalid_reach_the_turn() -> None:
    """Full path: transport → _handle_frame → the parked MessageTurn."""
    client, transport = make_client()
    await client.connect()

    turn = client.send_message(session_id="sess-1", message="quote please")
    req_id = transport.last_sent()["requestId"]

    collected: list = []

    async def iterate() -> None:
        async for ev in turn:
            collected.append(ev)

    task = asyncio.create_task(iterate())
    await asyncio.sleep(0)

    transport.emit(retarget(FIXTURES["interaction_required_event"]["instance"], req_id))
    transport.emit(retarget(FIXTURES["interaction_invalid_event"]["instance"], req_id))
    # Terminate the turn so the iterator completes.
    transport.emit(_terminal(req_id))
    await turn
    await asyncio.wait_for(task, timeout=30.0)

    types = [e.type for e in collected]
    assert "interaction_required" in types, f"interaction_required was dropped; got {types}"
    assert "interaction_invalid" in types, f"interaction_invalid was dropped; got {types}"
    assert types.index("interaction_required") < types.index("interaction_invalid")

    park = collected[types.index("interaction_required")]
    assert park.data.data.kind == "identity_intake"
    assert park.data.data.interaction_id == "88888888-8888-8888-8888-888888888888"

    invalid = collected[types.index("interaction_invalid")]
    assert [e.field for e in invalid.data.data.errors] == ["email"]


async def test_stream_preamble_and_reasoning_reach_the_turn(validator: ProtocolValidator) -> None:
    """Both are emitted by the production Rust server today and have no conformance
    fixture, so the frames are validated against their own schemas here before being
    dispatched — a frame the spec would reject proves nothing."""
    client, transport = make_client()
    await client.connect()

    turn = client.send_message(session_id="sess-1", message="think about it")
    req_id = transport.last_sent()["requestId"]

    frames = {
        "stream_preamble": ("events/stream-preamble.schema.json", "Looking that up…"),
        "stream_reasoning": ("events/stream-reasoning.schema.json", "let me think"),
    }
    built = {}
    for ev_type, (ref, token) in frames.items():
        frame = {
            "type": ev_type,
            "requestId": req_id,
            "token": token,
            "data": {"requestId": req_id, "token": token},
        }
        result = validator.validate_at(ref, frame)
        assert result.valid, f"{ev_type} test frame is not spec-valid: {format_errors(result.errors)}"
        built[ev_type] = frame

    collected: list = []

    async def iterate() -> None:
        async for ev in turn:
            collected.append(ev)

    task = asyncio.create_task(iterate())
    await asyncio.sleep(0)

    for frame in built.values():
        transport.emit(frame)
    transport.emit(_terminal(req_id))
    await turn
    await asyncio.wait_for(task, timeout=30.0)

    types = [e.type for e in collected]
    assert "stream_preamble" in types, f"stream_preamble was dropped; got {types}"
    assert "stream_reasoning" in types, f"stream_reasoning was dropped; got {types}"


# ─────────────────────────── the submit verb ──────────────────────────────────
async def test_submit_interaction_frame_is_spec_valid(validator: ProtocolValidator) -> None:
    client, transport = make_client()
    await client.connect()

    client.submit_interaction(
        session_id="22222222-2222-2222-2222-222222222222",
        request_id="req-a1b2c3d4-0004",
        interaction_id="88888888-8888-8888-8888-888888888888",
        kind="identity_intake",
        values={"name": "Alice Example", "email": "alice@example.com", "phone": "+15551234567"},
    )
    sent = transport.last_sent()
    assert sent["action"] == "submit_interaction"
    assert sent["interactionId"] == "88888888-8888-8888-8888-888888888888"
    assert "declined" not in sent, "declined must stay off the wire when not declining"
    result = validator.validate_at(SUBMIT_REF, sent)
    assert result.valid, format_errors(result.errors)

    # Matches the canonical fixture's shape key-for-key.
    assert set(sent) == set(FIXTURES["submit_interaction_request"]["instance"])


async def test_submit_interaction_declined_omits_values(validator: ProtocolValidator) -> None:
    client, transport = make_client()
    await client.connect()

    client.submit_interaction(
        session_id="22222222-2222-2222-2222-222222222222",
        request_id="req-a1b2c3d4-0004",
        interaction_id="88888888-8888-8888-8888-888888888888",
        values={"name": "ignored"},
        declined=True,
    )
    sent = transport.last_sent()
    assert sent["declined"] is True
    assert "values" not in sent, "values must be omitted when declining"
    result = validator.validate_at(SUBMIT_REF, sent)
    assert result.valid, format_errors(result.errors)


async def test_submit_interaction_carries_choices_values(validator: ProtocolValidator) -> None:
    """The ONE verb serves a second kind unchanged — choices needs no new method."""
    client, transport = make_client()
    await client.connect()

    values = FIXTURES["choices_values"]["instance"]
    client.submit_interaction(
        session_id="22222222-2222-2222-2222-222222222222",
        request_id="req-a1b2c3d4-0004",
        interaction_id="88888888-8888-8888-8888-888888888888",
        kind="choices",
        values=values,
    )
    sent = transport.last_sent()
    assert sent["values"] == values, "choices values lost data in transit"
    result = validator.validate_at(SUBMIT_REF, sent)
    assert result.valid, format_errors(result.errors)


async def test_unknown_event_is_ignored_not_fatal() -> None:
    """The OTHER half of the contract, and the reason the drift guard must not be
    "fixed" by making unknown types raise: the stream_reasoning schema says clients
    that do not recognize an event MUST ignore it. A frame from a NEWER server that
    this build predates has to be dropped silently, leaving the turn healthy."""
    client, transport = make_client()
    await client.connect()

    turn = client.send_message(session_id="sess-1", message="hi")
    req_id = transport.last_sent()["requestId"]

    collected: list = []

    async def iterate() -> None:
        async for ev in turn:
            collected.append(ev)

    task = asyncio.create_task(iterate())
    await asyncio.sleep(0)

    transport.emit(
        {
            "type": "stream_hologram",
            "requestId": req_id,
            "token": "from the future",
            "data": {"requestId": req_id, "token": "from the future"},
        }
    )
    transport.emit(
        {"type": "stream_token", "requestId": req_id, "token": "real", "data": {"requestId": req_id, "token": "real"}}
    )
    transport.emit(_terminal(req_id))
    await turn
    await asyncio.wait_for(task, timeout=30.0)

    types = [e.type for e in collected]
    assert "stream_hologram" not in types, "an unrecognised event type must not be surfaced to consumers"
    # The unknown frame must not have derailed the turn: the known events still land.
    assert "stream_token" in types, f"unknown frame derailed the turn; got {types}"
    assert "eventual_response" in types, f"turn did not settle normally; got {types}"

    # And the guard rejects it rather than the parser raising on a malformed frame.
    assert not is_server_event({"type": "stream_hologram", "data": {}})
