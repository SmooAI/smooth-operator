/**
 * The cross-pod backplane seam.
 *
 * In a multi-pod deployment, a connection on pod A may need events fanned in from
 * a turn processed on pod B (server-initiated pushes, shared sessions). The Rust
 * server abstracts this as a backplane with `attach(connId, sink)` /
 * `detach(connId)` (in-memory single-process, or Redis/NATS cross-pod). The
 * dispatcher/runner here are single-pod, but the connection lifecycle still runs a
 * `detach` after its read loop exits — so the seam is wired for a real backplane to
 * drop into later.
 *
 * The MVP ships an in-memory no-op backplane; the interface is what matters.
 */
import type { Frame } from './protocol.js';

/** A sink the backplane can push fanned-in events into for a given connection. */
export type BackplaneSink = (frame: Frame) => void;

export interface Backplane {
    /** Register a connection so events addressed to it can be delivered. */
    attach(connId: string, sink: BackplaneSink): Promise<void>;
    /** Deregister a connection. ALWAYS run after a connection's read loop exits. */
    detach(connId: string): Promise<void>;
    /**
     * Fan a frame out to a connection's attached sink, returning how many sinks it
     * reached: 1 when attached, 0 when not. The count is what
     * `POST /admin/publish` reports as `delivered`, so it must never claim a
     * delivery that did not happen.
     *
     * OPTIONAL so a third-party backplane that predates this stays valid; the
     * admin route answers 501 when a backplane does not implement it.
     */
    publish?(connId: string, frame: Frame): number;
}

/** In-memory single-process backplane. The MVP default; no cross-pod fan-in. */
export class InMemoryBackplane implements Backplane {
    private readonly sinks = new Map<string, BackplaneSink>();

    async attach(connId: string, sink: BackplaneSink): Promise<void> {
        this.sinks.set(connId, sink);
    }

    async detach(connId: string): Promise<void> {
        this.sinks.delete(connId);
    }

    /** Deliver to connId's sink if attached. Returns the number of sinks reached. */
    publish(connId: string, frame: Frame): number {
        const sink = this.sinks.get(connId);
        if (!sink) return 0;
        sink(frame);
        return 1;
    }

    /** Whether a connection is currently attached (used by tests to assert detach ran). */
    has(connId: string): boolean {
        return this.sinks.has(connId);
    }

    /** Number of currently-attached connections (used by tests to assert detach ran). */
    get size(): number {
        return this.sinks.size;
    }
}
