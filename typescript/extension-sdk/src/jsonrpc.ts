/**
 * A symmetric JSON-RPC 2.0 peer over a message-passing transport.
 *
 * Both ends of SEP are peers: each issues requests, replies to the other's
 * requests, and sends fire-and-forget notifications. This one `Peer` class is
 * the shared core — the extension runtime and the in-process test host are both
 * just a `Peer` with different handlers. It is transport-agnostic: it emits
 * frame objects via `send` and is fed inbound frames via `receive`; a codec
 * turns those into ndjson lines (see `transport.ts`).
 *
 * Cancellation is wired both ways:
 * - Outbound: pass an `AbortSignal`; on abort the Peer sends `$/cancel { id }`
 *   and rejects the pending request.
 * - Inbound: a request handler receives an `AbortSignal` that fires when the
 *   remote sends `$/cancel` for that request's id.
 */
import { errorCode, method } from './protocol.js';

export interface JsonRpcRequest {
    jsonrpc: '2.0';
    id: number | string;
    method: string;
    params?: unknown;
}
export interface JsonRpcNotification {
    jsonrpc: '2.0';
    method: string;
    params?: unknown;
}
export interface JsonRpcSuccess {
    jsonrpc: '2.0';
    id: number | string;
    result: unknown;
}
export interface JsonRpcError {
    jsonrpc: '2.0';
    id: number | string;
    error: { code: number; message: string; data?: unknown };
}
export type JsonRpcFrame = JsonRpcRequest | JsonRpcNotification | JsonRpcSuccess | JsonRpcError;

/** An error carrying a JSON-RPC error code, thrown for a remote error reply. */
export class RpcError extends Error {
    constructor(
        public readonly code: number,
        message: string,
        public readonly data?: unknown,
    ) {
        super(message);
        this.name = 'RpcError';
    }
}

export type RequestHandler = (params: unknown, signal: AbortSignal) => Promise<unknown> | unknown;
export type NotificationHandler = (params: unknown) => void;

interface Pending {
    resolve: (value: unknown) => void;
    reject: (err: Error) => void;
    onAbort?: () => void;
    signal?: AbortSignal;
}

export interface PeerOptions {
    /** Emit a frame to the remote. */
    send: (frame: JsonRpcFrame) => void;
    /** Called for any inbound method with no registered handler. */
    onUnhandled?: (frame: JsonRpcRequest | JsonRpcNotification) => void;
}

export class Peer {
    private nextId = 1;
    private readonly pending = new Map<number | string, Pending>();
    private readonly requestHandlers = new Map<string, RequestHandler>();
    private readonly notificationHandlers = new Map<string, NotificationHandler>();
    /** In-flight inbound requests we can cancel when the remote sends `$/cancel`. */
    private readonly inflight = new Map<number | string, AbortController>();
    private closed = false;

    constructor(private readonly opts: PeerOptions) {}

    setRequestHandler(name: string, handler: RequestHandler): void {
        this.requestHandlers.set(name, handler);
    }

    setNotificationHandler(name: string, handler: NotificationHandler): void {
        this.notificationHandlers.set(name, handler);
    }

    /** Issue a request; resolves with the remote's result or rejects with RpcError. */
    request<T = unknown>(name: string, params?: unknown, signal?: AbortSignal): Promise<T> {
        if (this.closed) return Promise.reject(new Error('peer is closed'));
        const id = this.nextId++;
        return new Promise<T>((resolve, reject) => {
            if (signal?.aborted) {
                reject(new RpcError(errorCode.Cancelled, 'cancelled before send'));
                return;
            }
            const onAbort = signal
                ? () => {
                      const p = this.pending.get(id);
                      if (!p) return;
                      this.pending.delete(id);
                      this.opts.send({ jsonrpc: '2.0', method: method.CANCEL, params: { id } });
                      reject(new RpcError(errorCode.Cancelled, 'cancelled'));
                  }
                : undefined;
            this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, onAbort, signal });
            signal?.addEventListener('abort', onAbort!, { once: true });
            this.opts.send({ jsonrpc: '2.0', id, method: name, params });
        });
    }

    /** Send a fire-and-forget notification. */
    notify(name: string, params?: unknown): void {
        if (this.closed) return;
        this.opts.send({ jsonrpc: '2.0', method: name, params });
    }

    /** Feed one inbound frame. */
    receive(frame: JsonRpcFrame): void {
        if ('id' in frame && 'method' in frame) {
            void this.handleRequest(frame);
        } else if ('method' in frame) {
            this.handleNotification(frame);
        } else if ('id' in frame) {
            this.handleResponse(frame);
        }
    }

    /** Reject every pending request; used on transport close. */
    close(reason = 'peer closed'): void {
        this.closed = true;
        for (const [id, p] of this.pending) {
            if (p.onAbort && p.signal) p.signal.removeEventListener('abort', p.onAbort);
            p.reject(new Error(reason));
            this.pending.delete(id);
        }
        for (const ctrl of this.inflight.values()) ctrl.abort();
        this.inflight.clear();
    }

    private async handleRequest(frame: JsonRpcRequest): Promise<void> {
        const handler = this.requestHandlers.get(frame.method);
        if (!handler) {
            this.opts.onUnhandled?.(frame);
            this.opts.send({ jsonrpc: '2.0', id: frame.id, error: { code: errorCode.MethodNotFound, message: `method not found: ${frame.method}` } });
            return;
        }
        const ctrl = new AbortController();
        this.inflight.set(frame.id, ctrl);
        try {
            const result = await handler(frame.params, ctrl.signal);
            if (ctrl.signal.aborted) {
                this.opts.send({ jsonrpc: '2.0', id: frame.id, error: { code: errorCode.Cancelled, message: 'cancelled' } });
            } else {
                this.opts.send({ jsonrpc: '2.0', id: frame.id, result: result ?? {} });
            }
        } catch (err) {
            if (ctrl.signal.aborted) {
                this.opts.send({ jsonrpc: '2.0', id: frame.id, error: { code: errorCode.Cancelled, message: 'cancelled' } });
            } else if (err instanceof RpcError) {
                this.opts.send({ jsonrpc: '2.0', id: frame.id, error: { code: err.code, message: err.message, data: err.data } });
            } else {
                this.opts.send({ jsonrpc: '2.0', id: frame.id, error: { code: errorCode.InternalError, message: err instanceof Error ? err.message : String(err) } });
            }
        } finally {
            this.inflight.delete(frame.id);
        }
    }

    private handleNotification(frame: JsonRpcNotification): void {
        // `$/cancel` aborts the matching in-flight inbound request.
        if (frame.method === method.CANCEL) {
            const id = (frame.params as { id?: number | string } | undefined)?.id;
            if (id !== undefined) this.inflight.get(id)?.abort();
            return;
        }
        const handler = this.notificationHandlers.get(frame.method);
        if (handler) handler(frame.params);
        else this.opts.onUnhandled?.(frame);
    }

    private handleResponse(frame: JsonRpcSuccess | JsonRpcError): void {
        const p = this.pending.get(frame.id);
        if (!p) return; // late reply for a cancelled/unknown request — drop it.
        this.pending.delete(frame.id);
        if (p.onAbort && p.signal) p.signal.removeEventListener('abort', p.onAbort);
        if ('error' in frame) p.reject(new RpcError(frame.error.code, frame.error.message, frame.error.data));
        else p.resolve(frame.result);
    }
}
