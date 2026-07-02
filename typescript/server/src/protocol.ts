/**
 * Builders for the server→client protocol event frames.
 *
 * The TypeScript port of the C# `ProtocolEvents.cs` (and the Rust reference
 * server's `protocol.rs`). The JSON shapes mirror them byte-for-byte — including
 * the triple-nested `eventual_response.data.data` — so they validate against the
 * same `spec/events/*.schema.json` and the `spec/conformance/fixtures.json`
 * golden messages.
 *
 * Every builder returns a plain JSON-serializable object; the transport stringifies
 * it before writing it to the socket.
 */

/** A JSON object frame ready to be serialized to the wire. */
export type Frame = Record<string, unknown>;

/** One auto-context citation: the source a grounded answer leaned on. */
export interface Citation {
    id: string;
    title: string;
    url?: string;
    snippet: string;
    score: number;
}

const nowMs = (): number => Date.now();

/** `pong` reply to a `ping`. */
export function pong(requestId?: string): Frame {
    const ev: Frame = { type: 'pong', timestamp: nowMs() };
    if (requestId !== undefined) ev.requestId = requestId;
    return ev;
}

/** A synchronous response carried in an `immediate_response` event's `data`. */
export function immediateResponse(requestId: string | undefined, status: number, message: string, data: Record<string, unknown>): Frame {
    const ev: Frame = {
        type: 'immediate_response',
        status,
        message,
        data,
        timestamp: nowMs(),
    };
    if (requestId !== undefined) ev.requestId = requestId;
    return ev;
}

/** One incremental assistant text delta. */
export function streamToken(requestId: string, token: string): Frame {
    return {
        type: 'stream_token',
        requestId,
        token,
        data: { requestId, token },
        timestamp: nowMs(),
    };
}

/** A per-node state snapshot (tool call / tool result on the live turn). */
export function streamChunk(requestId: string, node: string, state: Record<string, unknown>): Frame {
    return {
        type: 'stream_chunk',
        requestId,
        node,
        data: { requestId, node, state },
        timestamp: nowMs(),
    };
}

/**
 * The terminal turn event. Matches the Rust/C# shape: a triple-nested `data.data`
 * carrying `messageId`, the agent `response`, `needsEscalation`, and — only when
 * non-empty — the `citations` array.
 */
export function eventualResponse(
    requestId: string,
    status: number,
    messageId: string,
    response: Record<string, unknown>,
    needsEscalation: boolean,
    citations?: Citation[],
): Frame {
    const inner: Record<string, unknown> = { messageId, response, needsEscalation };
    if (citations && citations.length > 0) {
        inner.citations = citations.map((c) => citation(c.id, c.title, c.url, c.snippet, c.score));
    }

    return {
        type: 'eventual_response',
        requestId,
        status,
        data: { requestId, status, data: inner },
        timestamp: nowMs(),
    };
}

/**
 * A protocol-level error event. The connection survives; this just signals it.
 *
 * The `{ code, message }` descriptor is duplicated at the envelope level (`error`)
 * AND nested under `data.error` — matching the Python/Rust reference servers and the
 * `error.schema.json` shape. The envelope-level `error` is what clients (and the
 * conformance corpus) pattern-match on; `data.error` is kept for wire
 * backward-compatibility.
 */
/**
 * `write_confirmation_required` — emitted mid-turn when the agent calls a
 * state-mutating tool that requires explicit human approval before it runs. The turn
 * is **parked** (the engine's `HumanGate` awaits the verdict) until the client replies
 * with a `confirm_tool_action` action carrying the same `requestId` and an `approved`
 * boolean.
 *
 * Wire shape matches `spec/events/write-confirmation-required.schema.json` and the
 * Rust/Python reference servers byte-for-byte: the `requestId` echoes the originating
 * `send_message`, and the prompt detail is double-nested under
 * `data.data.{toolId, actionDescription}`. `toolId` is an opaque correlation handle
 * (the tool name — a turn parks one tool at a time); `actionDescription` is the
 * human-readable prompt the client renders.
 */
export function writeConfirmationRequired(requestId: string, toolId: string, actionDescription: string): Frame {
    return {
        type: 'write_confirmation_required',
        requestId,
        data: {
            requestId,
            data: { toolId, actionDescription },
        },
        timestamp: nowMs(),
    };
}

/**
 * `otp_verification_required` — emitted after a turn's auth gate refused an
 * `end_user` tool on an unverified session and the host has an OTP service
 * installed. Tells the client to collect a one-time code. Wire shape matches
 * `spec/events/otp-verification-required.schema.json` (double-nested `data.data`).
 * `availableChannels` are the delivery channels the server can offer given the
 * session's known contacts (`email` / `sms`).
 */
export function otpVerificationRequired(requestId: string, toolId: string, actionDescription: string, availableChannels: string[], authLevel: string): Frame {
    return {
        type: 'otp_verification_required',
        requestId,
        data: {
            requestId,
            data: { toolId, actionDescription, availableChannels, authLevel },
        },
        timestamp: nowMs(),
    };
}

/**
 * `otp_sent` — acknowledgement that a code was dispatched to the caller via the
 * chosen channel. Wire shape matches `spec/events/otp-sent.schema.json`. The
 * `maskedDestination` comes from the host — the server never sees the full address
 * or the code.
 */
export function otpSent(requestId: string, channel: string, maskedDestination: string): Frame {
    return {
        type: 'otp_sent',
        requestId,
        data: {
            requestId,
            data: { channel, maskedDestination },
        },
        timestamp: nowMs(),
    };
}

/**
 * `otp_verified` — emitted when a `verify_otp` attempt succeeds; the session is now
 * identity-verified and the client re-sends its message to run the gated tool (the
 * reference server does not park/auto-resume the original turn). Wire shape matches
 * `spec/events/otp-verified.schema.json`.
 */
export function otpVerified(requestId: string, message: string): Frame {
    return {
        type: 'otp_verified',
        requestId,
        data: {
            requestId,
            data: { message },
        },
        timestamp: nowMs(),
    };
}

/**
 * `otp_invalid` — emitted when a `verify_otp` attempt is rejected, carrying the
 * host's remaining-attempt count (0 ⇒ locked) and an optional machine-readable
 * `error` (omitted when the host couldn't determine a cause, per the schema). Wire
 * shape matches `spec/events/otp-invalid.schema.json`.
 */
export function otpInvalid(requestId: string, error: string | undefined, attemptsRemaining: number, message: string): Frame {
    const inner: Record<string, unknown> = { attemptsRemaining, message };
    // Optional per spec: only emit `error` when the host determined a cause.
    if (error !== undefined) inner.error = error;
    return {
        type: 'otp_invalid',
        requestId,
        data: {
            requestId,
            data: inner,
        },
        timestamp: nowMs(),
    };
}

export function error(requestId: string | undefined, code: string, message: string): Frame {
    const descriptor = { code, message };
    const data: Record<string, unknown> = { error: descriptor };
    if (requestId !== undefined) data.requestId = requestId;
    const ev: Frame = {
        type: 'error',
        error: descriptor,
        data,
        timestamp: nowMs(),
    };
    if (requestId !== undefined) ev.requestId = requestId;
    return ev;
}

/** A minimal `GeneralAgentResponse` wrapping the agent's reply text. */
export function generalResponse(reply: string): Record<string, unknown> {
    return {
        responseParts: [reply],
        customerHappinessScore: 0.5,
        needsSatisfactionScore: 0.5,
        requestSummary: '',
        resolutionStatus: 'in_progress',
        suggestedNextActions: [],
    };
}

/** Build a single citation object (omits `url` when there isn't a web source). */
export function citation(id: string, title: string, url: string | undefined, snippet: string, score: number): Record<string, unknown> {
    const c: Record<string, unknown> = { id, title, snippet, score };
    if (url !== undefined) c.url = url;
    return c;
}
