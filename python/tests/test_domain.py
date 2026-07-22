"""Domain round-trip tests via the generated pydantic models.

Exercises the camelCase-wire / snake_case-Python contract: parse from the wire
form, read idiomatic snake_case attributes, and re-emit the camelCase wire form with
``model_dump(by_alias=True)``.
"""

from __future__ import annotations

from smooth_operator import (
    ConversationMessage,
    Message,
    Participant,
    Session,
    parse_event,
)


def test_participant_ai_agent_round_trips() -> None:
    wire = {
        "id": "55555555-5555-5555-5555-555555555555",
        "conversationId": "33333333-3333-3333-3333-333333333333",
        "organizationId": "99999999-9999-9999-9999-999999999999",
        "type": "ai-agent",
        "internalId": "agent-internal-1",
        "name": "Aria",
        "createdAt": "2026-06-01T12:00:00Z",
        "updatedAt": "2026-06-01T12:00:00Z",
    }
    p = Participant.model_validate(wire)
    assert p.type.value == "ai-agent"
    assert str(p.conversation_id) == "33333333-3333-3333-3333-333333333333"
    assert p.internal_id == "agent-internal-1"

    dumped = p.model_dump(by_alias=True, exclude_none=True, mode="json")
    assert dumped["conversationId"] == "33333333-3333-3333-3333-333333333333"
    assert dumped["organizationId"] == "99999999-9999-9999-9999-999999999999"
    assert dumped["type"] == "ai-agent"
    # camelCase aliases on the wire, never the snake_case Python names.
    assert "conversation_id" not in dumped


def test_message_inbound_round_trips() -> None:
    wire = {
        "id": "66666666-6666-6666-6666-666666666666",
        "direction": "inbound",
        "content": {
            "items": [{"type": "text", "text": "What is the status of my order?"}],
            "text": "What is the status of my order?",
        },
        "from": {"id": "44444444-4444-4444-4444-444444444444", "type": "user", "name": "Alice"},
        "createdAt": "2026-06-01T12:00:01Z",
    }
    m = Message.model_validate(wire)
    assert m.direction.value == "inbound"
    assert m.content.text == "What is the status of my order?"
    assert m.content.items[0].type.value == "text"
    # `from` is a Python keyword → aliased to from_.
    assert m.from_ is not None
    assert m.from_.type == "user"

    dumped = m.model_dump(by_alias=True, exclude_none=True, mode="json")
    assert dumped["direction"] == "inbound"
    assert dumped["from"]["type"] == "user"  # re-emits the `from` wire key
    assert "from_" not in dumped


def test_session_thread_id_round_trips() -> None:
    wire = {
        "sessionId": "22222222-2222-2222-2222-222222222222",
        "conversationId": "33333333-3333-3333-3333-333333333333",
        # Required on Session since spec PR #97. The generated model only began
        # enforcing it once _generated.py was regenerated against current spec/.
        "organizationId": "77777777-7777-7777-7777-777777777777",
        "agentId": "11111111-1111-1111-1111-111111111111",
        "agentName": "Aria",
        "userParticipantId": "44444444-4444-4444-4444-444444444444",
        "agentParticipantId": "55555555-5555-5555-5555-555555555555",
        "threadId": "thread-abc-123",
        "status": "active",
    }
    s = Session.model_validate(wire)
    assert s.thread_id == "thread-abc-123"
    assert s.status.value == "active"

    dumped = s.model_dump(by_alias=True, exclude_none=True, mode="json")
    assert dumped["threadId"] == "thread-abc-123"
    assert "thread_id" not in dumped


def test_populate_by_name_accepts_snake_case_input() -> None:
    # Because populate_by_name=True, the Python snake_case names are also accepted.
    s = Session(
        session_id="22222222-2222-2222-2222-222222222222",
        conversation_id="33333333-3333-3333-3333-333333333333",
        organization_id="77777777-7777-7777-7777-777777777777",
        agent_id="11111111-1111-1111-1111-111111111111",
        agent_name="Aria",
        user_participant_id="44444444-4444-4444-4444-444444444444",
        agent_participant_id="55555555-5555-5555-5555-555555555555",
        thread_id="thread-xyz",
    )
    assert s.thread_id == "thread-xyz"


def test_conversation_message_wire_subset_parses() -> None:
    wire = {
        "id": "66666666-6666-6666-6666-666666666666",
        "direction": "outbound",
        "content": {"text": "Your order shipped.", "structuredResponse": None},
        "createdAt": "2026-06-01T12:00:02Z",
    }
    cm = ConversationMessage.model_validate(wire)
    assert cm.direction.value == "outbound"
    assert cm.content.text == "Your order shipped."


def test_parse_event_builds_a_typed_discriminated_event() -> None:
    frame = {
        "type": "stream_token",
        "requestId": "req-1",
        "token": "Hi",
        "data": {"requestId": "req-1", "token": "Hi"},
    }
    ev = parse_event(frame)
    # Discriminated union picked the StreamToken model.
    assert ev.type == "stream_token"
    assert ev.token == "Hi"
    assert ev.data.token == "Hi"
