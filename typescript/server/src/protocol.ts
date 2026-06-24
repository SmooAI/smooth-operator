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

/** A protocol-level error event. The connection survives; this just signals it. */
export function error(requestId: string | undefined, code: string, message: string): Frame {
    const ev: Frame = {
        type: 'error',
        data: { error: { code, message } },
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
