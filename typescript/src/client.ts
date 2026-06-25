/**
 * SmoothAgentClient — a minimal, idiomatic, transport-agnostic client for the
 * smooth-operator WebSocket protocol.
 *
 * Design goals
 * ------------
 *  - **Transport-agnostic.** The client never touches a real socket directly; it
 *    talks to an injectable {@link Transport}. The default ({@link WebSocketTransport})
 *    uses the global `WebSocket`, but tests inject a mock and Node can inject `ws`.
 *  - **Request/response correlation by `requestId`.** Every action gets a generated
 *    `requestId`; the client routes incoming events back to the originating call.
 *  - **Streaming as an async iterator.** `sendMessage` returns a {@link MessageTurn}
 *    that is both awaitable (resolves with the terminal `eventual_response`) and
 *    async-iterable (yields each `stream_token` / `stream_chunk` / HITL event in
 *    order). This models the `stream_token`/`stream_chunk` → `eventual_response`
 *    flow without forcing a callback style on the caller.
 *  - **No live server required.** Correctness is fully unit-testable with a mock
 *    transport (see `test/client.test.ts`).
 */
import {
    WebSocketTransport,
    type Transport,
    type WebSocketFactory,
} from './transport.js';
import type {
    ClientAction,
    CreateConversationSessionRequest,
    CreateConversationSessionResponse,
    EventualResponse,
    GetMessagesRequest,
    GetMessagesResponse,
    GetSessionRequest,
    GetSessionResponse,
    SendMessageRequest,
    ServerEvent,
} from './types.js';
import { isServerEvent } from './types.js';

export interface SmoothAgentClientOptions {
    /** WebSocket URL, e.g. `wss://realtime.prod.smooth-agent.dev`. */
    url: string;
    /**
     * Optional connection auth token for token-gated servers (e.g. the local-flavor
     * server). When set, the token is appended to the connection URL as a `?token=`
     * query parameter — browsers can't set custom headers on a WebSocket handshake,
     * so the token rides the query string, which is where the server reads it from.
     * Any existing query string on `url` is preserved. This applies to the default
     * transport only; if a custom {@link transport} is injected, supply the token to
     * that transport yourself.
     */
    token?: string;
    /** Inject a transport (for tests / non-browser runtimes). Defaults to a WebSocket transport. */
    transport?: Transport;
    /** Inject a WebSocket factory used by the default transport (e.g. the `ws` package on Node). */
    webSocketFactory?: WebSocketFactory;
    /** Generate request IDs. Defaults to `crypto.randomUUID()` with a `req-` prefix. */
    generateRequestId?: () => string;
    /** Per-request timeout in ms for non-streaming actions. Default 30000. */
    requestTimeout?: number;
    /**
     * Overall timeout in ms for a streaming `sendMessage` turn: if the server accepts
     * the message but never emits a terminal `eventual_response` / `error`, the turn
     * rejects with a {@link TurnTimeoutError} instead of hanging forever. Default
     * 120000. Set to 0 (or a negative number) to disable.
     */
    turnTimeout?: number;
}

/** Events that terminate a streaming turn (success or error). */
const TURN_TERMINAL = new Set(['eventual_response', 'error']);

/** A timeout that yields no terminal event. */
class RequestTimeoutError extends Error {
    constructor(requestId: string, ms: number) {
        super(`Request ${requestId} timed out after ${ms}ms`);
        this.name = 'RequestTimeoutError';
    }
}

/**
 * A streaming turn that received no terminal `eventual_response` / `error` within the
 * configured {@link SmoothAgentClientOptions.turnTimeout}. The turn rejects with this
 * and its async iteration throws it, so a stuck server can never hang the caller.
 */
export class TurnTimeoutError extends Error {
    readonly requestId: string;
    constructor(requestId: string, ms: number) {
        super(`Turn ${requestId} timed out after ${ms}ms without a terminal response`);
        this.name = 'TurnTimeoutError';
        this.requestId = requestId;
    }
}

/** A protocol-level error event surfaced as a throwable. */
export class ProtocolError extends Error {
    readonly code: string;
    readonly requestId?: string;
    constructor(code: string, message: string, requestId?: string) {
        super(message);
        this.name = 'ProtocolError';
        this.code = code;
        this.requestId = requestId;
    }
}

/** Internal record for an in-flight single-response request. */
interface PendingRequest {
    resolve: (event: ServerEvent) => void;
    reject: (err: unknown) => void;
    timer: ReturnType<typeof setTimeout> | undefined;
}

/**
 * A streaming message turn. Await it for the terminal {@link EventualResponse},
 * or async-iterate it to receive every intermediate event in arrival order.
 *
 * ```ts
 * const turn = client.sendMessage({ sessionId, message: 'hi' });
 * for await (const ev of turn) {
 *   if (ev.type === 'stream_token') process.stdout.write(ev.token ?? '');
 * }
 * const final = await turn; // EventualResponse
 * ```
 */
export class MessageTurn implements AsyncIterable<ServerEvent>, PromiseLike<EventualResponse> {
    /** The requestId this turn is correlated on. */
    readonly requestId: string;

    private readonly queue: ServerEvent[] = [];
    private waiter: {
        resolve: (result: IteratorResult<ServerEvent>) => void;
        reject: (err: unknown) => void;
    } | null = null;
    private done = false;
    private finalEvent: EventualResponse | null = null;
    private error: unknown = null;
    private readonly settled: Promise<EventualResponse>;
    private settle!: (value: EventualResponse) => void;
    private fail!: (err: unknown) => void;
    private readonly onClose: () => void;
    private timeoutTimer: ReturnType<typeof setTimeout> | undefined;

    constructor(requestId: string, onClose: () => void, turnTimeout = 0) {
        this.requestId = requestId;
        this.onClose = onClose;
        this.settled = new Promise<EventualResponse>((resolve, reject) => {
            this.settle = resolve;
            this.fail = reject;
        });
        // Avoid unhandled-rejection noise if the caller only iterates and never awaits.
        this.settled.catch(() => {});
        // Bound the turn: a server that accepts the message but never emits a terminal
        // event must not hang the caller forever.
        if (turnTimeout > 0) {
            this.timeoutTimer = setTimeout(() => {
                this.finish(null, new TurnTimeoutError(this.requestId, turnTimeout));
            }, turnTimeout);
        }
    }

    /** Feed an event into the turn (called by the client's dispatcher). */
    push(event: ServerEvent): void {
        if (this.done) return;

        if (event.type === 'error') {
            const code = event.data?.error?.code ?? 'INTERNAL_ERROR';
            const message = event.data?.error?.message ?? 'Unknown protocol error';
            this.deliver(event);
            this.finish(null, new ProtocolError(code, message, this.requestId));
            return;
        }

        this.deliver(event);

        if (event.type === 'eventual_response') {
            this.finish(event, null);
        }
    }

    /** Force-close the turn (e.g. on disconnect) with an error. */
    abort(err: unknown): void {
        if (this.done) return;
        this.finish(null, err);
    }

    private deliver(event: ServerEvent): void {
        if (this.waiter) {
            const w = this.waiter;
            this.waiter = null;
            w.resolve({ value: event, done: false });
        } else {
            this.queue.push(event);
        }
    }

    private finish(final: EventualResponse | null, err: unknown): void {
        if (this.done) return;
        this.done = true;
        this.finalEvent = final;
        this.error = err;
        if (this.timeoutTimer) {
            clearTimeout(this.timeoutTimer);
            this.timeoutTimer = undefined;
        }
        this.onClose();

        if (err) this.fail(err);
        else if (final) this.settle(final);

        // Release any pending iterator waiter now that the stream has ended. On error
        // the parked next() must *reject* (mirroring the queued-error path in next())
        // so a pure `for await` consumer sees the terminal error thrown instead of a
        // silent, indistinguishable `{ done: true }`.
        if (this.waiter) {
            const w = this.waiter;
            this.waiter = null;
            if (err) {
                w.reject(err);
            } else {
                w.resolve({ value: undefined as never, done: true });
            }
        }
    }

    [Symbol.asyncIterator](): AsyncIterator<ServerEvent> {
        return {
            next: (): Promise<IteratorResult<ServerEvent>> => {
                if (this.queue.length > 0) {
                    return Promise.resolve({ value: this.queue.shift()!, done: false });
                }
                if (this.done) {
                    if (this.error) return Promise.reject(this.error);
                    return Promise.resolve({ value: undefined as never, done: true });
                }
                return new Promise<IteratorResult<ServerEvent>>((resolve, reject) => {
                    this.waiter = { resolve, reject };
                });
            },
        };
    }

    // PromiseLike — `await turn` resolves with the EventualResponse.
    then<TResult1 = EventualResponse, TResult2 = never>(
        onfulfilled?: ((value: EventualResponse) => TResult1 | PromiseLike<TResult1>) | null,
        onrejected?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null,
    ): PromiseLike<TResult1 | TResult2> {
        return this.settled.then(onfulfilled, onrejected);
    }
}

export class SmoothAgentClient {
    private readonly transport: Transport;
    private readonly generateRequestId: () => string;
    private readonly requestTimeout: number;
    private readonly turnTimeout: number;

    /** requestId → single-response waiter (create_session, get_session, ping, …). */
    private readonly pending = new Map<string, PendingRequest>();
    /** requestId → active streaming turn (send_message, and HITL resumes). */
    private readonly turns = new Map<string, MessageTurn>();
    /** Unsolicited-event listeners (keepalive, server-push). */
    private readonly listeners = new Set<(event: ServerEvent) => void>();

    private unsubscribe: Array<() => void> = [];

    constructor(options: SmoothAgentClientOptions) {
        this.transport =
            options.transport ??
            new WebSocketTransport(withConnectionToken(options.url, options.token), options.webSocketFactory);
        this.requestTimeout = options.requestTimeout ?? 30_000;
        this.turnTimeout = options.turnTimeout ?? 120_000;
        this.generateRequestId =
            options.generateRequestId ??
            (() => `req-${(globalThis.crypto?.randomUUID?.() ?? Math.random().toString(36).slice(2))}`);

        this.unsubscribe.push(this.transport.onMessage((data) => this.handleFrame(data)));
        this.unsubscribe.push(
            this.transport.onClose(() => this.failAll(new Error('Transport closed'))),
        );
    }

    /** Open the underlying transport. */
    async connect(): Promise<void> {
        await this.transport.connect();
    }

    /** Close the transport and reject all in-flight work. */
    disconnect(reason = 'client disconnect'): void {
        this.failAll(new Error(reason));
        for (const u of this.unsubscribe) u();
        this.unsubscribe = [];
        this.transport.close(1000, reason);
    }

    /** Subscribe to unsolicited / uncorrelated server events (e.g. keepalive). */
    onEvent(listener: (event: ServerEvent) => void): () => void {
        this.listeners.add(listener);
        return () => this.listeners.delete(listener);
    }

    // ───────────────────────────── Actions ─────────────────────────────────

    /** Start a new conversation session. Resolves with the session descriptor. */
    async createConversationSession(
        req: Omit<CreateConversationSessionRequest, 'action' | 'requestId'>,
    ): Promise<CreateConversationSessionResponse> {
        const event = await this.request({ action: 'create_conversation_session', ...req });
        return extractImmediateData<CreateConversationSessionResponse>(event);
    }

    /** Fetch a session snapshot by ID. */
    async getSession(req: Omit<GetSessionRequest, 'action' | 'requestId'>): Promise<GetSessionResponse> {
        const event = await this.request({ action: 'get_session', ...req });
        return extractImmediateData<GetSessionResponse>(event);
    }

    /** Fetch a page of conversation messages. */
    async getMessages(req: Omit<GetMessagesRequest, 'action' | 'requestId'>): Promise<GetMessagesResponse> {
        const event = await this.request({ action: 'get_conversation_messages', ...req });
        return extractImmediateData<GetMessagesResponse>(event);
    }

    /** Keepalive ping. Resolves with the server timestamp from the `pong` event. */
    async ping(): Promise<number> {
        const event = await this.request({ action: 'ping' });
        if (event.type === 'pong') return event.timestamp ?? event.data?.timestamp ?? Date.now();
        return Date.now();
    }

    /**
     * Submit a user message and return a {@link MessageTurn}: await it for the
     * terminal `eventual_response`, or async-iterate it for the streaming events.
     */
    sendMessage(req: Omit<SendMessageRequest, 'action' | 'requestId'>): MessageTurn {
        const requestId = this.generateRequestId();
        const turn = new MessageTurn(requestId, () => this.turns.delete(requestId), this.turnTimeout);
        this.turns.set(requestId, turn);
        try {
            this.transport.send(JSON.stringify({ action: 'send_message', requestId, ...req }));
        } catch (err) {
            this.turns.delete(requestId);
            turn.abort(err);
        }
        return turn;
    }

    /**
     * Approve or reject a pending tool write, resuming the paused turn identified
     * by `requestId`. The resumed streaming events flow back into the original
     * {@link MessageTurn} for that `requestId`.
     */
    confirmToolAction(req: { sessionId: string; requestId: string; approved: boolean }): void {
        this.transport.send(JSON.stringify({ action: 'confirm_tool_action', ...req }));
    }

    /**
     * Submit an OTP code, resuming the paused turn identified by `requestId`.
     * The resumed streaming events flow back into the original {@link MessageTurn}.
     */
    verifyOtp(req: { sessionId: string; requestId: string; code: string }): void {
        this.transport.send(JSON.stringify({ action: 'verify_otp', ...req }));
    }

    // ─────────────────────────── Internals ─────────────────────────────────

    /** Send an action that expects a single correlated response event. */
    private request(action: Omit<ClientAction, 'requestId'> & { requestId?: string }): Promise<ServerEvent> {
        const requestId = action.requestId ?? this.generateRequestId();
        const frame = { ...action, requestId } as ClientAction;

        return new Promise<ServerEvent>((resolve, reject) => {
            const timer =
                this.requestTimeout > 0
                    ? setTimeout(() => {
                          this.pending.delete(requestId);
                          reject(new RequestTimeoutError(requestId, this.requestTimeout));
                      }, this.requestTimeout)
                    : undefined;

            this.pending.set(requestId, { resolve, reject, timer });
            try {
                this.transport.send(JSON.stringify(frame));
            } catch (err) {
                if (timer) clearTimeout(timer);
                this.pending.delete(requestId);
                reject(err);
            }
        });
    }

    /** Parse and route an incoming frame to the right consumer. */
    private handleFrame(data: string): void {
        let frame: unknown;
        try {
            frame = JSON.parse(data);
        } catch {
            return; // ignore malformed frames
        }
        if (!isServerEvent(frame)) return;
        const event = frame;
        const requestId = event.requestId;

        // 1. Streaming turn? Route every related event into it.
        if (requestId && this.turns.has(requestId)) {
            this.turns.get(requestId)!.push(event);
            return;
        }

        // 2. Single-response request awaiting resolution?
        if (requestId && this.pending.has(requestId)) {
            const pending = this.pending.get(requestId)!;
            this.pending.delete(requestId);
            if (pending.timer) clearTimeout(pending.timer);

            if (event.type === 'error') {
                const code = event.data?.error?.code ?? 'INTERNAL_ERROR';
                const message = event.data?.error?.message ?? 'Unknown protocol error';
                pending.reject(new ProtocolError(code, message, requestId));
            } else {
                pending.resolve(event);
            }
            return;
        }

        // 3. Unsolicited / uncorrelated event (keepalive, server push).
        for (const l of this.listeners) l(event);
    }

    private failAll(err: unknown): void {
        for (const [, p] of this.pending) {
            if (p.timer) clearTimeout(p.timer);
            p.reject(err);
        }
        this.pending.clear();
        for (const [, turn] of this.turns) turn.abort(err);
        this.turns.clear();
    }
}

/**
 * Merge a connection auth `token` into a WebSocket URL as a `?token=` query param,
 * preserving any existing query string. Returns `url` unchanged when no token is
 * given, so the no-token path is byte-for-byte identical to before. Uses `URL` /
 * `URLSearchParams` so an existing `?foo=bar` becomes `?foo=bar&token=…` (and the
 * value is properly percent-encoded) rather than a naive `?`/`&` string-concat.
 *
 * Falls back to manual concatenation if `url` is not absolute (so `URL` can't parse
 * it) — e.g. a relative or mock URL used in tests.
 */
function withConnectionToken(url: string, token?: string): string {
    if (!token) return url;
    try {
        const parsed = new URL(url);
        parsed.searchParams.set('token', token);
        return parsed.toString();
    } catch {
        const separator = url.includes('?') ? '&' : '?';
        return `${url}${separator}token=${encodeURIComponent(token)}`;
    }
}

/** Pull the typed `data` payload out of an `immediate_response` event. */
function extractImmediateData<T>(event: ServerEvent): T {
    if (event.type === 'immediate_response') return event.data as T;
    // Some servers may answer a non-streaming action with the payload elsewhere;
    // fall back to `data` if present.
    if ('data' in event && event.data && typeof event.data === 'object') return event.data as T;
    throw new ProtocolError('UNEXPECTED_EVENT', `Expected immediate_response, got "${event.type}"`, event.requestId);
}
