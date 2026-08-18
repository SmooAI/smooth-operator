"""Ergonomic, hand-curated types layered on top of the generated ones.

The generated models in :mod:`smooth_operator._generated` are a faithful 1:1
reflection of the JSON Schemas — one pydantic model per schema/``$def``. They are
correct but flat: there is no single discriminated union over the wire frames, and
datamodel-code-generator names the deeply-nested ``data.data`` payloads
``Data``/``Data1``/``Data2`` etc.

This module fixes both. It:

  * re-exports the generated models under stable, intention-revealing names
    (including readable aliases for the nested ``data`` payloads),
  * defines the two enums consumers want (``ActionType`` over ``action`` and
    ``EventType`` over ``type``),
  * builds a **discriminated** ``ServerEvent`` union (pydantic ``Field(discriminator=
    "type")``) plus a ``parse_event`` helper, and a parallel ``ClientAction`` union,
  * provides ``is_server_event`` / ``is_client_action`` guards.

Naming convention
-----------------
The wire format is camelCase (``requestId``, ``sessionId``). Every model uses
**snake_case Python attributes** with camelCase **aliases** and
``populate_by_name = True``, so you read/write ``event.request_id`` in Python while
``model_dump(by_alias=True)`` / ``model_dump_json(by_alias=True)`` round-trips the
camelCase wire form. Inbound parsing accepts either form.
"""

from __future__ import annotations

from enum import StrEnum
from typing import Annotated, Literal, Union

from pydantic import Field, TypeAdapter

from . import _generated as _g

# ───────────────────────────── Re-exported models ──────────────────────────────
# Shared / envelope
ErrorObject = _g.ErrorObject
ActionEnvelope = _g.ActionEnvelope
EventEnvelope = _g.EventEnvelope

# Action requests
CreateConversationSessionRequest = _g.CreateConversationSessionRequest
SendMessageRequest = _g.SendMessageRequest
GetSessionRequest = _g.GetSessionRequest
GetMessagesRequest = _g.GetMessagesRequest
ConfirmToolActionRequest = _g.ConfirmToolActionRequest
VerifyOtpRequest = _g.VerifyOtpRequest
CancelRequest = _g.CancelRequest
PingRequest = _g.PingRequest
AuthContext = _g.AuthContext

# Action response payloads (carried in immediate_response.data)
CreateConversationSessionResponse = _g.CreateConversationSessionResponse
GetSessionResponse = _g.GetSessionResponse
GetMessagesResponse = _g.GetMessagesResponse
SendMessageResponse = _g.SendMessageResponse
PongResponse = _g.PongResponse
GeneralAgentResponse = _g.GeneralAgentResponse
ConversationMessage = _g.ConversationMessage

# Server events
ImmediateResponse = _g.ImmediateResponse
EventualResponse = _g.EventualResponse
StreamChunk = _g.StreamChunk
StreamToken = _g.StreamToken
Keepalive = _g.Keepalive
WriteConfirmationRequired = _g.WriteConfirmationRequired
OtpVerificationRequired = _g.OtpVerificationRequired
OtpSent = _g.OtpSent
OtpVerified = _g.OtpVerified
OtpInvalid = _g.OtpInvalid
Cancelled = _g.Cancelled
Pong = _g.Pong

# The generated `error` event model is named ``Error`` — which shadows the builtin.
# Re-export under an unambiguous name.
ErrorEvent = _g.Error

# Domain entities
Conversation = _g.Conversation
Participant = _g.Participant
Message = _g.Message
MessageContent = _g.MessageContent
ContentItem = _g.ContentItem
Session = _g.Session
Checkpoint = _g.Checkpoint

# Readable aliases for the nested ``data.data`` payloads (datamodel-code-generator
# names them DataN by structural position). Surfacing them lets callers type-hint
# the inner payloads without reaching for the cryptic generated names.
EventualResponseData = _g.Data1  # eventual_response.data
EventualResponseInner = _g.Data2  # eventual_response.data.data
ErrorEventData = _g.Data  # error.data
KeepaliveData = _g.Data3  # keepalive.data
StreamChunkData = _g.Data13  # stream_chunk.data
StreamChunkState = _g.State  # stream_chunk.data.state
StreamTokenData = _g.Data14  # stream_token.data


# ───────────────────────────── Discriminators ──────────────────────────────────
class ActionType(StrEnum):
    """Every client→server ``action`` discriminator value."""

    create_conversation_session = "create_conversation_session"
    send_message = "send_message"
    get_session = "get_session"
    get_conversation_messages = "get_conversation_messages"
    confirm_tool_action = "confirm_tool_action"
    verify_otp = "verify_otp"
    cancel = "cancel"
    ping = "ping"


class EventType(StrEnum):
    """Every server→client ``type`` discriminator value."""

    immediate_response = "immediate_response"
    eventual_response = "eventual_response"
    stream_chunk = "stream_chunk"
    stream_token = "stream_token"
    keepalive = "keepalive"
    write_confirmation_required = "write_confirmation_required"
    otp_verification_required = "otp_verification_required"
    otp_sent = "otp_sent"
    otp_verified = "otp_verified"
    otp_invalid = "otp_invalid"
    cancelled = "cancelled"
    error = "error"
    pong = "pong"


ACTION_TYPES: frozenset[str] = frozenset(a.value for a in ActionType)
EVENT_TYPES: frozenset[str] = frozenset(e.value for e in EventType)


# ───────────────────────────── Server events ───────────────────────────────────
# A pydantic *discriminated* union over the ``type`` field. Each generated event
# model carries a ``Literal['<type>']`` on its ``type`` attribute, so pydantic can
# pick the right model in one pass with no trial-and-error.
ServerEvent = Annotated[
    Union[
        ImmediateResponse,
        EventualResponse,
        StreamChunk,
        StreamToken,
        Keepalive,
        WriteConfirmationRequired,
        OtpVerificationRequired,
        OtpSent,
        OtpVerified,
        OtpInvalid,
        Cancelled,
        ErrorEvent,
        Pong,
    ],
    Field(discriminator="type"),
]
"""Discriminated union over every server→client frame, keyed on ``type``."""

_SERVER_EVENT_ADAPTER: TypeAdapter[ServerEvent] = TypeAdapter(ServerEvent)


# ───────────────────────────── Client actions ──────────────────────────────────
ClientAction = Annotated[
    Union[
        CreateConversationSessionRequest,
        SendMessageRequest,
        GetSessionRequest,
        GetMessagesRequest,
        ConfirmToolActionRequest,
        VerifyOtpRequest,
        CancelRequest,
        PingRequest,
    ],
    Field(discriminator="action"),
]
"""Discriminated union over every client→server frame, keyed on ``action``."""

_CLIENT_ACTION_ADAPTER: TypeAdapter[ClientAction] = TypeAdapter(ClientAction)


def parse_event(frame: dict | str | bytes) -> ServerEvent:
    """Parse a raw wire frame (dict, JSON string, or bytes) into the concrete,
    typed :data:`ServerEvent` model selected by its ``type`` discriminator.

    Accepts both camelCase (wire) and snake_case (Python) keys.
    """
    if isinstance(frame, (str, bytes)):
        return _SERVER_EVENT_ADAPTER.validate_json(frame)
    return _SERVER_EVENT_ADAPTER.validate_python(frame)


def parse_action(frame: dict | str | bytes) -> ClientAction:
    """Parse a raw wire frame into the concrete, typed :data:`ClientAction` model
    selected by its ``action`` discriminator."""
    if isinstance(frame, (str, bytes)):
        return _CLIENT_ACTION_ADAPTER.validate_json(frame)
    return _CLIENT_ACTION_ADAPTER.validate_python(frame)


def is_server_event(frame: object) -> bool:
    """True if ``frame`` looks like any server event (has a known ``type``)."""
    if isinstance(frame, _g.BaseModel):
        return getattr(frame, "type", None) in EVENT_TYPES
    if isinstance(frame, dict):
        return frame.get("type") in EVENT_TYPES
    return False


def is_client_action(frame: object) -> bool:
    """True if ``frame`` looks like any client action (has a known ``action``)."""
    if isinstance(frame, _g.BaseModel):
        action = getattr(frame, "action", None)
        return (action.value if isinstance(action, StrEnum) else action) in ACTION_TYPES
    if isinstance(frame, dict):
        return frame.get("action") in ACTION_TYPES
    return False


# Convenience: literal event-type strings for narrowing in user code.
EventTypeLiteral = Literal[
    "immediate_response",
    "eventual_response",
    "stream_chunk",
    "stream_token",
    "keepalive",
    "write_confirmation_required",
    "otp_verification_required",
    "otp_sent",
    "otp_verified",
    "otp_invalid",
    "cancelled",
    "error",
    "pong",
]
