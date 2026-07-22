/**
 * Maps the smooth-operator protocol onto a WebSocket endpoint — the deployable
 * surface of the TypeScript service.
 *
 * The TS port of the C# `SmoothOperatorWebSocketExtensions.cs` and the Rust
 * server's axum `/ws` upgrade + connection loop. Per connection it:
 *
 *  - resolves the `?token=` slot into an {@link AccessContext} (browsers can't set
 *    WebSocket headers) and binds a {@link FrameDispatcher} to it;
 *  - attaches to the backplane;
 *  - runs a read loop that RACES "the shared cancel fired" against "the next inbound
 *    frame", awaiting the turn dispatch INSIDE the frame branch so an in-flight turn
 *    finishes (graceful drain) — preferring cancel on ties;
 *  - feeds outbound events through a SINGLE writer (one socket = one writer; `ws`
 *    `send` must not be called concurrently);
 *  - always runs a backplane `detach` after the loop exits (detach-after-loop).
 *
 * `SIGTERM` / `SIGINT` stop accepting new connections and trigger the shared cancel,
 * so a rolling deploy drains in flight turns instead of cutting them.
 */
import { createServer, type IncomingMessage, type Server as HttpServer } from 'node:http';
import { WebSocketServer, type WebSocket } from 'ws';

import { ANONYMOUS_ACCESS } from './auth.js';
import { Backplane, InMemoryBackplane } from './backplane.js';
import type { AgentConfigResolver } from './agentConfig.js';
import type { SessionAuthenticator } from './toolGating.js';
import type { OtpService } from './otp.js';
import { type AccessKnowledge, FrameDispatcher } from './frameDispatcher.js';
import type { ModelCeilingResolver } from './modelCeiling.js';
import type { Frame } from './protocol.js';
import type { ChatClientLike, Tool, ToolHook } from '@smooai/smooth-operator-core';
import type { AuthVerifier } from './auth.js';
import { NoAuthVerifier } from './auth.js';
import { InMemorySessionStore, type SessionStore } from './sessionStore.js';

export interface ServerOptions {
    /** The OpenAI-compatible engine client (gateway in prod, a mock in tests). */
    chatClient: ChatClientLike;
    store?: SessionStore;
    knowledge?: AccessKnowledge;
    auth?: AuthVerifier;
    backplane?: Backplane;
    systemPrompt?: string;
    /**
     * Tools the agent may call during a turn (default none). Each is an engine
     * {@link Tool}; the dispatcher forwards them to the turn runner, which passes
     * them straight to the agent. Empty by default, so behaviour is unchanged.
     */
    tools?: Tool[];
    /**
     * Consumer-supplied tool-call surveillance {@link ToolHook}s (default none). The
     * builder seam a host uses to plug surveillance/redaction into EVERY turn's tool
     * registry: each hook's `preCall` runs before a tool executes (a throw blocks it)
     * and its `postCall` runs after with a mutable result it may redact. Forwarded
     * verbatim through the dispatcher to each turn's engine `toolHooks`. Unlike
     * {@link tools}, hooks bypass the per-agent enabled-tools filter — they see every
     * call. Empty ⇒ behaviour unchanged. Mirrors the Rust `LocalServerBuilder`'s hook
     * seam feeding the per-turn `ToolRegistry`.
     */
    toolHooks?: ToolHook[];
    /**
     * Tool-name patterns gated behind write-confirmation HITL (default empty → no
     * gating, behavior unchanged). When a turn calls a tool whose name contains one of
     * these, the server parks the turn and emits `write_confirmation_required` until the
     * client replies with `confirm_tool_action`.
     */
    confirmTools?: string[];
    /**
     * SMOODEV-590 — resolves a session's `agentId` into its per-agent config
     * (instructions, conversationWorkflow, greeting, personality, tool allow-list).
     * Undefined → every agent uses the server/org default prompt + tools (unchanged).
     */
    agentConfig?: AgentConfigResolver;
    /** The cheap model id the workflow judge uses (default {@link DEFAULT_JUDGE_MODEL}). */
    judgeModel?: string;
    /**
     * SMOODEV-590 — resolves whether a conversation's session is identity-verified,
     * for `end_user` tool-auth gating on public agents. Absent → fail-closed.
     */
    sessionAuthenticator?: SessionAuthenticator;
    /**
     * End-user OTP identity-verification seam. When set, a turn whose auth gate refuses
     * an `end_user` tool on an unverified session (with a known contact) offers an
     * OTP flow, and the `verify_otp` action validates submitted codes. Absent → the
     * fail-closed default (refuse, no OTP). The server never holds a code.
     */
    otpService?: OtpService;
    /** Model id for turns (default the engine's own default); forwarded to each connection's dispatcher. */
    model?: string;
    /**
     * Best-effort per-model output-ceiling resolver (from the gateway's `/model/info`).
     * When set, each turn clamps `max_tokens` to what the model can physically emit
     * (EPIC th-1cc9fa). Absent (tests, keyless local) ⇒ unclamped, behaviour unchanged.
     */
    modelCeiling?: ModelCeilingResolver;
    /** WS path to mount on (default `/ws`). */
    path?: string;
}

/**
 * A running server: its bound port + a `close()` that triggers the shared cancel
 * (draining in-flight turns) and awaits a clean shutdown.
 */
export interface RunningServer {
    readonly port: number;
    readonly url: string;
    /** The shared cancel — exposed so a graceful-drain test can fire it directly. */
    readonly drainSignal: AbortSignal;
    /** Trigger drain + stop accepting + close. Resolves once fully shut down. */
    close(): Promise<void>;
}

/**
 * Build (but do not start) the HTTP + WebSocket server. Returns the underlying
 * `http.Server`, the `WebSocketServer`, and the shared {@link AbortController} so a
 * host can wire its own signal handling. Most callers want {@link serve} /
 * {@link serveLocal} instead.
 */
export function buildServer(options: ServerOptions): {
    http: HttpServer;
    wss: WebSocketServer;
    drain: AbortController;
    backplane: Backplane;
} {
    const store = options.store ?? new InMemorySessionStore();
    const auth = options.auth ?? new NoAuthVerifier();
    const backplane = options.backplane ?? new InMemoryBackplane();
    const path = options.path ?? '/ws';

    // One shared cancel for the whole server — the single source, default uncancelled.
    // SIGTERM/SIGINT (or close()) fire it; every connection's read loop watches it.
    const drain = new AbortController();

    const http = createServer((_req, res) => {
        // Plain HTTP isn't part of the protocol surface — a tiny health response.
        res.writeHead(426, { 'content-type': 'text/plain' });
        res.end('Upgrade Required: connect over WebSocket\n');
    });

    const wss = new WebSocketServer({ server: http, path });

    wss.on('connection', (socket: WebSocket, req: IncomingMessage) => {
        const token = tokenFromRequest(req);
        const access = auth.resolve(token) ?? ANONYMOUS_ACCESS;
        const dispatcher = new FrameDispatcher({
            store,
            chatClient: options.chatClient,
            knowledge: options.knowledge,
            access,
            systemPrompt: options.systemPrompt,
            tools: options.tools,
            toolHooks: options.toolHooks,
            confirmTools: options.confirmTools,
            agentConfig: options.agentConfig,
            judgeModel: options.judgeModel,
            sessionAuthenticator: options.sessionAuthenticator,
            otpService: options.otpService,
            model: options.model,
            modelCeiling: options.modelCeiling,
        });
        // Fire-and-forget the per-connection loop; it owns the socket's lifecycle.
        void runConnection(socket, dispatcher, backplane, drain.signal);
    });

    return { http, wss, drain, backplane };
}

/** Boot the server on `host:port` and return a handle. Port `0` picks an ephemeral one. */
export async function serve(options: ServerOptions & { host?: string; port?: number }): Promise<RunningServer> {
    const { http, wss, drain, backplane } = buildServer(options);
    const host = options.host ?? '127.0.0.1';
    const port = options.port ?? 0;

    await new Promise<void>((resolve, reject) => {
        http.once('error', reject);
        http.listen(port, host, () => {
            http.off('error', reject);
            resolve();
        });
    });

    const address = http.address();
    const boundPort = typeof address === 'object' && address ? address.port : port;

    // SIGTERM/SIGINT: stop accepting + trigger drain. Registered once per server;
    // removed on close so repeated boot/shutdown in tests doesn't leak listeners.
    const onSignal = () => void shutdown();
    process.once('SIGTERM', onSignal);
    process.once('SIGINT', onSignal);

    let closing: Promise<void> | undefined;
    const shutdown = (): Promise<void> => {
        if (closing) return closing;
        closing = (async () => {
            process.off('SIGTERM', onSignal);
            process.off('SIGINT', onSignal);
            // Trigger the shared cancel first so in-flight read loops drain.
            drain.abort();
            // Stop accepting new connections.
            await new Promise<void>((resolve) => wss.close(() => resolve()));
            await new Promise<void>((resolve) => http.close(() => resolve()));
        })();
        return closing;
    };

    return {
        port: boundPort,
        url: `ws://${host}:${boundPort}${options.path ?? '/ws'}`,
        drainSignal: drain.signal,
        close: shutdown,
    };
}

/**
 * The LOCAL deployment flavor — a zero-config, in-memory, auth-off server,
 * embeddable in-process. The TS analog of the Rust `serve_local` / `LocalServer`
 * and the C# in-memory host. Everything in memory, loopback bind, no auth.
 */
export async function serveLocal(options: {
    chatClient: ChatClientLike;
    host?: string;
    port?: number;
    knowledge?: AccessKnowledge;
    model?: string;
    modelCeiling?: ModelCeilingResolver;
    /** Tools the agent may call during a turn (default none). */
    tools?: Tool[];
    /**
     * Consumer-supplied tool-call surveillance {@link ToolHook}s (default none) — the
     * embed seam a host (the smooth daemon) uses to install Narc/redaction on every
     * turn's tool registry. Forwarded straight to {@link ServerOptions.toolHooks}.
     */
    toolHooks?: ToolHook[];
}): Promise<RunningServer> {
    return serve({
        chatClient: options.chatClient,
        store: new InMemorySessionStore(),
        auth: new NoAuthVerifier(),
        backplane: new InMemoryBackplane(),
        knowledge: options.knowledge,
        tools: options.tools,
        toolHooks: options.toolHooks,
        model: options.model,
        modelCeiling: options.modelCeiling,
        host: options.host ?? '127.0.0.1',
        port: options.port ?? 0,
    });
}

/**
 * Drive ONE connection: the graceful-drain read loop + the single outbound writer.
 *
 * The read loop races the shared `drainSignal` against the next inbound frame. On a
 * frame, the dispatch is awaited INSIDE the frame branch, so an in-flight turn
 * finishes before the loop re-checks for cancel. On a tie (a frame arrives exactly
 * as cancel fires) cancel wins. After the loop exits — for ANY reason — the
 * backplane `detach` runs (detach-after-loop).
 */
async function runConnection(socket: WebSocket, dispatcher: FrameDispatcher, backplane: Backplane, drainSignal: AbortSignal): Promise<void> {
    const connId = cryptoRandomId();

    // Single outbound writer: a queue drained by one async pump. `ws.send` is never
    // called concurrently. Mirrors the Rust sink_tx + writer split / the C# channel.
    // The writer wakes on a level-triggered Signal so a frame enqueued before the
    // pump parks is never lost (a classic lost-wakeup that bit the naive version).
    const outbound: Frame[] = [];
    const writerSignal = new Signal();
    let writerStop = false;
    const sink = (frame: Frame): void => {
        outbound.push(frame);
        writerSignal.notify();
    };

    const writer = (async () => {
        for (;;) {
            const frame = outbound.shift();
            if (frame === undefined) {
                if (writerStop) break;
                await writerSignal.wait();
                continue;
            }
            try {
                await sendFrame(socket, frame);
            } catch {
                break; // socket gone
            }
        }
    })();

    await backplane.attach(connId, sink);

    // Inbound frames arrive on the socket's 'message' event; buffer them and hand
    // them to the read loop one at a time (the loop awaits each dispatch). The reader
    // wakes on its own level-triggered Signal — and the shared `drainSignal` also
    // notifies it, so the read loop races "cancel" against "next frame".
    const inboundQueue: string[] = [];
    const readerSignal = new Signal();
    let socketClosed = false;

    const onDrain = () => readerSignal.notify();
    drainSignal.addEventListener('abort', onDrain, { once: true });

    socket.on('message', (data: Buffer | ArrayBuffer | Buffer[]) => {
        inboundQueue.push(frameToString(data));
        readerSignal.notify();
    });
    socket.on('close', () => {
        socketClosed = true;
        readerSignal.notify();
    });
    socket.on('error', () => {
        socketClosed = true;
        readerSignal.notify();
    });

    try {
        // The read loop: race cancel vs the next frame.
        for (;;) {
            // On a tie (a frame is queued AND cancel fired), prefer cancel: drain
            // rather than start another turn.
            if (drainSignal.aborted) break;

            const raw = inboundQueue.shift();
            if (raw === undefined) {
                if (socketClosed) break;
                // Park until a frame arrives, the socket closes, or cancel fires —
                // all three notify `readerSignal`.
                await readerSignal.wait();
                continue;
            }

            // Await the dispatch INSIDE the frame branch so an in-flight turn finishes
            // even if cancel fires while it runs (the turn cooperatively stops streaming
            // further events via the signal, but the terminal event still lands).
            await dispatcher.dispatch(raw, sink, drainSignal);
        }
    } finally {
        // Detach-after-loop: ALWAYS deregister from the backplane, regardless of why
        // the loop exited (cancel, socket close, or error).
        drainSignal.removeEventListener('abort', onDrain);
        await backplane.detach(connId);

        // A turn parked on a write-confirmation must unpark before we can finish: reject
        // outstanding confirmations (fail closed — a write is never auto-approved on
        // disconnect), then await every in-flight spawned turn so its `eventual_response`
        // is enqueued before the writer stops (preserves the graceful-drain "in-flight
        // turn finishes" contract now that turns run as background tasks). No-op when no
        // turn is parked / in flight.
        // Client DISCONNECTED mid-turn: abort the in-flight turn — no client remains to
        // receive its output, and its partial reply must not be persisted. Distinct from
        // a graceful drain (SIGTERM), which falls through with `socketClosed === false`
        // and deliberately lets the turn finish below.
        if (socketClosed) dispatcher.cancelActiveTurn();

        dispatcher.rejectPendingConfirmations();
        await dispatcher.waitForTurns();

        // Stop the writer and let it flush what's queued, then close the socket one-way.
        writerStop = true;
        writerSignal.notify();
        await writer;

        try {
            socket.close(1000, 'bye');
        } catch {
            // socket already gone
        }
    }
}

/**
 * A level-triggered async signal: `notify()` records that work is pending and wakes
 * a parked `wait()`; a `wait()` after a `notify()` returns immediately and consumes
 * the pending flag. This is the lost-wakeup-safe replacement for a bare "set a
 * resolver, call it later" handoff (where a notify before the park is dropped).
 */
class Signal {
    private pending = false;
    private resolve: (() => void) | undefined;

    notify(): void {
        this.pending = true;
        const r = this.resolve;
        this.resolve = undefined;
        r?.();
    }

    wait(): Promise<void> {
        if (this.pending) {
            this.pending = false;
            return Promise.resolve();
        }
        return new Promise<void>((resolve) => {
            this.resolve = () => {
                this.pending = false;
                resolve();
            };
        });
    }
}

function tokenFromRequest(req: IncomingMessage): string | undefined {
    try {
        const url = new URL(req.url ?? '', 'ws://localhost');
        return url.searchParams.get('token') ?? undefined;
    } catch {
        return undefined;
    }
}

function frameToString(data: Buffer | ArrayBuffer | Buffer[]): string {
    if (typeof data === 'string') return data;
    if (Array.isArray(data)) return Buffer.concat(data).toString('utf8');
    if (data instanceof ArrayBuffer) return Buffer.from(data).toString('utf8');
    return (data as Buffer).toString('utf8');
}

function sendFrame(socket: WebSocket, frame: Frame): Promise<void> {
    return new Promise<void>((resolve, reject) => {
        socket.send(JSON.stringify(frame), (err) => (err ? reject(err) : resolve()));
    });
}

function cryptoRandomId(): string {
    // Lazy import keeps this module's top free of node:crypto for tree-shaking.
    return globalThis.crypto?.randomUUID?.() ?? Math.random().toString(36).slice(2);
}
