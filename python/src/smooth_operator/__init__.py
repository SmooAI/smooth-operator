"""smooth_operator — Python protocol types + native async client for the smooth-operator
WebSocket protocol.

The protocol contract is defined by the language-neutral JSON Schemas in ``spec/``.
The generated pydantic models (:mod:`smooth_operator._generated`) are committed so
consumers don't need the generator; :mod:`smooth_operator.types` layers the ergonomic
discriminated unions and helpers on top.
"""

from __future__ import annotations

from .client import (
    MessageTurn,
    ProtocolError,
    RequestTimeoutError,
    SmoothAgentClient,
    TurnTimeoutError,
)
from .transport import Transport, TransportState, WebSocketTransport
from .types import (
    ACTION_TYPES,
    EVENT_TYPES,
    ActionType,
    AuthContext,
    Cancelled,
    CancelRequest,
    Checkpoint,
    ClientAction,
    Conversation,
    ConversationMessage,
    CreateConversationSessionRequest,
    CreateConversationSessionResponse,
    ErrorEvent,
    ErrorObject,
    EventType,
    EventualResponse,
    GeneralAgentResponse,
    GetMessagesRequest,
    GetMessagesResponse,
    GetSessionRequest,
    GetSessionResponse,
    ImmediateResponse,
    Keepalive,
    Message,
    MessageContent,
    OtpInvalid,
    OtpSent,
    OtpVerificationRequired,
    OtpVerified,
    Participant,
    PingRequest,
    Pong,
    PongResponse,
    SendMessageRequest,
    SendMessageResponse,
    ServerEvent,
    Session,
    StreamChunk,
    StreamToken,
    WriteConfirmationRequired,
    is_client_action,
    is_server_event,
    parse_action,
    parse_event,
)
from .validate import (
    DEFAULT_SPEC_DIR,
    ProtocolValidator,
    ValidationResult,
    format_errors,
)

__all__ = [
    # client
    "SmoothAgentClient",
    "MessageTurn",
    "ProtocolError",
    "RequestTimeoutError",
    "TurnTimeoutError",
    # transport
    "Transport",
    "TransportState",
    "WebSocketTransport",
    # discriminators / unions / helpers
    "ActionType",
    "EventType",
    "ACTION_TYPES",
    "EVENT_TYPES",
    "ClientAction",
    "ServerEvent",
    "parse_event",
    "parse_action",
    "is_server_event",
    "is_client_action",
    # action requests
    "CreateConversationSessionRequest",
    "SendMessageRequest",
    "GetSessionRequest",
    "GetMessagesRequest",
    "CancelRequest",
    "PingRequest",
    "AuthContext",
    # response payloads
    "CreateConversationSessionResponse",
    "GetSessionResponse",
    "GetMessagesResponse",
    "SendMessageResponse",
    "PongResponse",
    "GeneralAgentResponse",
    "ConversationMessage",
    # events
    "ImmediateResponse",
    "EventualResponse",
    "StreamChunk",
    "StreamToken",
    "Keepalive",
    "WriteConfirmationRequired",
    "OtpVerificationRequired",
    "OtpSent",
    "OtpVerified",
    "OtpInvalid",
    "Cancelled",
    "ErrorEvent",
    "Pong",
    "ErrorObject",
    # domain
    "Conversation",
    "Participant",
    "Message",
    "MessageContent",
    "Session",
    "Checkpoint",
    # validation
    "ProtocolValidator",
    "ValidationResult",
    "DEFAULT_SPEC_DIR",
    "format_errors",
]
