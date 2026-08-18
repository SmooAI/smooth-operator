/**
 * Ergonomic, hand-curated types layered on top of the generated ones.
 *
 * The generated types in `./generated/types.ts` are a faithful 1:1 reflection of
 * the JSON Schemas — one interface per schema/`$def`. They are correct but flat:
 * there is no single discriminated union over the wire frames, and the generated
 * `Error` interface collides with the global `Error`.
 *
 * This module fixes both: it re-exports the generated types under stable names and
 * builds the two discriminated unions consumers actually want —
 * `ClientAction` (everything sent client→server, discriminated by `action`) and
 * `ServerEvent` (everything received server→client, discriminated by `type`).
 */
import type {
    CancelRequest,
    Cancelled,
    ConfirmToolActionRequest,
    CreateConversationSessionRequest,
    CreateConversationSessionResponse,
    Error as GeneratedErrorEvent,
    EventualResponse,
    GetMessagesRequest,
    GetMessagesResponse,
    GetSessionRequest,
    GetSessionResponse,
    ImmediateResponse,
    InteractionInvalid,
    InteractionRequired,
    Keepalive,
    OtpInvalid,
    OtpSent,
    OtpVerificationRequired,
    OtpVerified,
    PingRequest,
    Pong,
    SendMessageRequest,
    SendMessageResponse,
    StreamChunk,
    StreamPreamble,
    StreamToken,
    SubmitInteractionRequest,
    VerifyOtpRequest,
    WriteConfirmationRequired,
} from './generated/types.js';

export * from './generated/types.js';

/**
 * The generated `error` event interface is named `Error`, which shadows the
 * global `Error`. Re-export it under an unambiguous name.
 */
export type { GeneratedErrorEvent as ErrorEvent };

// ───────────────────────────── Discriminators ──────────────────────────────

/** Every client→server `action` discriminator value. */
export const ACTION_TYPES = [
    'create_conversation_session',
    'send_message',
    'get_session',
    'get_conversation_messages',
    'confirm_tool_action',
    'verify_otp',
    'submit_interaction',
    'cancel',
    'ping',
] as const;
export type ActionType = (typeof ACTION_TYPES)[number];

/** Every server→client `type` discriminator value. */
export const EVENT_TYPES = [
    'immediate_response',
    'eventual_response',
    'stream_chunk',
    'stream_token',
    'stream_preamble',
    'keepalive',
    'write_confirmation_required',
    'otp_verification_required',
    'otp_sent',
    'otp_verified',
    'otp_invalid',
    'interaction_required',
    'interaction_invalid',
    'cancelled',
    'error',
    'pong',
] as const;
export type EventType = (typeof EVENT_TYPES)[number];

// ───────────────────────────── Client actions ──────────────────────────────

/**
 * Discriminated union over every client→server action frame, keyed on `action`.
 * Narrow with `action.action === 'send_message'` to get the concrete request type.
 */
export type ClientAction =
    | CreateConversationSessionRequest
    | SendMessageRequest
    | GetSessionRequest
    | GetMessagesRequest
    | ConfirmToolActionRequest
    | VerifyOtpRequest
    | SubmitInteractionRequest
    | CancelRequest
    | PingRequest;

// ───────────────────────────── Server events ───────────────────────────────

/**
 * Discriminated union over every server→client event frame, keyed on `type`.
 * Narrow with `event.type === 'stream_token'` to get the concrete event type.
 */
export type ServerEvent =
    | ImmediateResponse
    | EventualResponse
    | StreamChunk
    | StreamToken
    | StreamPreamble
    | Keepalive
    | WriteConfirmationRequired
    | OtpVerificationRequired
    | OtpSent
    | OtpVerified
    | OtpInvalid
    | InteractionRequired
    | InteractionInvalid
    | Cancelled
    | GeneratedErrorEvent
    | Pong;

// ─────────────────────── Per-type narrowed convenience ──────────────────────

/** Map from event `type` → the concrete generated event interface. */
export interface ServerEventByType {
    immediate_response: ImmediateResponse;
    eventual_response: EventualResponse;
    stream_chunk: StreamChunk;
    stream_token: StreamToken;
    stream_preamble: StreamPreamble;
    keepalive: Keepalive;
    write_confirmation_required: WriteConfirmationRequired;
    otp_verification_required: OtpVerificationRequired;
    otp_sent: OtpSent;
    otp_verified: OtpVerified;
    otp_invalid: OtpInvalid;
    interaction_required: InteractionRequired;
    interaction_invalid: InteractionInvalid;
    cancelled: Cancelled;
    error: GeneratedErrorEvent;
    pong: Pong;
}

/** Map from action `action` → the concrete generated request interface. */
export interface ClientActionByType {
    create_conversation_session: CreateConversationSessionRequest;
    send_message: SendMessageRequest;
    get_session: GetSessionRequest;
    get_conversation_messages: GetMessagesRequest;
    confirm_tool_action: ConfirmToolActionRequest;
    verify_otp: VerifyOtpRequest;
    submit_interaction: SubmitInteractionRequest;
    cancel: CancelRequest;
    ping: PingRequest;
}

// ───────────────────────────── Type guards ─────────────────────────────────

/** True if `frame` is a server event of the given `type`, narrowing accordingly. */
export function isEvent<T extends EventType>(frame: unknown, type: T): frame is ServerEventByType[T] {
    return isServerEvent(frame) && frame.type === type;
}

/** True if `frame` looks like any server event (has a known `type` discriminator). */
export function isServerEvent(frame: unknown): frame is ServerEvent {
    return (
        typeof frame === 'object' &&
        frame !== null &&
        'type' in frame &&
        typeof (frame as { type: unknown }).type === 'string' &&
        (EVENT_TYPES as readonly string[]).includes((frame as { type: string }).type)
    );
}

/** True if `frame` looks like any client action (has a known `action` discriminator). */
export function isClientAction(frame: unknown): frame is ClientAction {
    return (
        typeof frame === 'object' &&
        frame !== null &&
        'action' in frame &&
        typeof (frame as { action: unknown }).action === 'string' &&
        (ACTION_TYPES as readonly string[]).includes((frame as { action: string }).action)
    );
}

// ──────────────────────── Decoded response payloads ────────────────────────

/**
 * The typed `data` payload returned in the `immediate_response` for each
 * non-streaming action. These are the generated `*Response` shapes, surfaced so
 * the client's typed methods can return them directly.
 */
export type {
    CreateConversationSessionResponse,
    GetSessionResponse,
    GetMessagesResponse,
    SendMessageResponse,
};
