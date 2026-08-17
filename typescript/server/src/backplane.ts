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

/**
 * A delivery target: one connection, or every connection for a session / user / org /
 * agent. Ports the Rust reference's `Target`.
 */
export interface Target {
    kind: 'connection' | 'session' | 'user' | 'org' | 'agent';
    id: string;
}

/** The five kinds a publish may name. Anything else is the caller's error. */
export const TARGET_KINDS: readonly Target['kind'][] = ['connection', 'session', 'user', 'org', 'agent'];

/**
 * Map key for a target. NUL-separated rather than `:` because an id can legitimately
 * contain a colon (an org name, an email), which would let two distinct targets collide.
 */
function targetKey(target: Target): string {
    return `${target.kind}\u0000${target.id}`;
}

export interface Backplane {
    /** Register a connection so events addressed to it can be delivered. */
    attach(connId: string, sink: BackplaneSink): Promise<void>;
    /** Deregister a connection. ALWAYS run after a connection's read loop exits. */
    detach(connId: string): Promise<void>;
    /**
     * Associate a connection with a target, so a publish to that target reaches it.
     * Idempotent; learned over the connection's life (user/org from auth at connect,
     * session/agent as sessions resolve).
     *
     * OPTIONAL alongside {@link publish} for the same back-compat reason.
     */
    associate?(connId: string, target: Target): void;
    /**
     * Fan a frame out to every connection associated with `target`, returning how many
     * sinks it reached. The count is what `POST /admin/publish` reports as `delivered`,
     * so it must never claim a delivery that did not happen.
     *
     * OPTIONAL so a third-party backplane that predates this stays valid; the
     * admin route answers 501 when a backplane does not implement it.
     */
    publish?(target: Target, frame: Frame): number;
}

/**
 * In-memory single-process backplane: connection sinks plus a target → connections index,
 * so all five target kinds resolve locally (the Rust reference's fan-out). No cross-pod
 * fan-in — that is the Redis/NATS seam.
 */
export class InMemoryBackplane implements Backplane {
    private readonly sinks = new Map<string, BackplaneSink>();
    /** target key → the conn ids associated with it, for publish fan-out. */
    private readonly byTarget = new Map<string, Set<string>>();
    /** conn id → its target keys, so detach tears every association down. */
    private readonly byConn = new Map<string, Set<string>>();

    async attach(connId: string, sink: BackplaneSink): Promise<void> {
        this.sinks.set(connId, sink);
        // Always reachable by its own connection id.
        this.associate(connId, { kind: 'connection', id: connId });
    }

    async detach(connId: string): Promise<void> {
        this.sinks.delete(connId);
        for (const key of this.byConn.get(connId) ?? []) {
            const conns = this.byTarget.get(key);
            if (!conns) continue;
            conns.delete(connId);
            // A leaked association would resolve to a dead socket and inflate `delivered`.
            if (conns.size === 0) this.byTarget.delete(key);
        }
        this.byConn.delete(connId);
    }

    associate(connId: string, target: Target): void {
        const key = targetKey(target);
        let conns = this.byTarget.get(key);
        if (!conns) this.byTarget.set(key, (conns = new Set()));
        conns.add(connId);

        let keys = this.byConn.get(connId);
        if (!keys) this.byConn.set(connId, (keys = new Set()));
        keys.add(key);
    }

    /** Deliver to every connection for `target`. Returns the number of sinks reached. */
    publish(target: Target, frame: Frame): number {
        let delivered = 0;
        for (const connId of this.byTarget.get(targetKey(target)) ?? []) {
            const sink = this.sinks.get(connId);
            if (!sink) continue;
            sink(frame);
            delivered++;
        }
        return delivered;
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
