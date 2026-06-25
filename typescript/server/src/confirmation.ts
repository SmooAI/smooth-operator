/**
 * Write-confirmation HITL — the pending-confirmation registry.
 *
 * When an agent turn calls a tool that requires human approval, the turn **parks**
 * inside the engine's {@link HumanGate} and the runner registers a resolver here,
 * keyed by `sessionId`. A subsequent `confirm_tool_action` frame on the same
 * connection looks the session up, resolves the deferred with the verdict, and the
 * parked turn resumes (runs the tool on approve; skips it with a rejection result
 * on deny).
 *
 * The TypeScript analog of the Rust `AppState` pending-confirmation map
 * (`register_confirmation` / `take_confirmation` / `clear_confirmation`), the Python
 * `ConfirmationRegistry`, and the C# pending-confirmation registry. Keyed by session
 * so each session has at most one outstanding confirmation; an empty registry means
 * no turn is parked (the default — behavior identical to before HITL).
 */

/** A deferred boolean: a promise plus its resolver, so the holder can settle it later. */
interface Deferred {
    /** The promise a parked turn awaits. `true` = approved, `false` = rejected. */
    readonly promise: Promise<boolean>;
    /** Settle the promise with the verdict. Idempotent — a second call is a no-op. */
    resolve(approved: boolean): void;
    /** True once the deferred has been settled. */
    settled(): boolean;
}

function makeDeferred(): Deferred {
    let resolveFn!: (approved: boolean) => void;
    let isSettled = false;
    const promise = new Promise<boolean>((resolve) => {
        resolveFn = resolve;
    });
    return {
        promise,
        resolve(approved: boolean): void {
            if (isSettled) return;
            isSettled = true;
            resolveFn(approved);
        },
        settled(): boolean {
            return isSettled;
        },
    };
}

/**
 * Tracks the in-flight write-confirmation each parked turn is waiting on.
 *
 * Single-threaded under the Node event loop: every method runs on the loop, so the
 * map needs no extra locking. One registry per connection (a `confirm_tool_action`
 * frame and the parked turn it resumes are always on the same connection).
 */
export class ConfirmationRegistry {
    /** `sessionId` → the deferred a parked turn awaits. At most one per session. */
    private readonly pending = new Map<string, Deferred>();

    /**
     * Register (and return) a fresh approval promise for `sessionId`.
     *
     * Any prior pending deferred for the session is rejected (resolved `false`)
     * first, so a stale parked turn can never be left dangling and the newest
     * confirmation always wins — mirrors the Rust `register_confirmation` taking over
     * a prior sender.
     */
    register(sessionId: string): Promise<boolean> {
        const prior = this.pending.get(sessionId);
        if (prior && !prior.settled()) prior.resolve(false);
        const deferred = makeDeferred();
        this.pending.set(sessionId, deferred);
        return deferred.promise;
    }

    /**
     * Resolve the parked turn for `sessionId` with the verdict.
     *
     * Returns `true` if a pending confirmation was resolved, `false` if none was
     * awaiting (a duplicate/stale `confirm_tool_action` → `NO_PENDING_CONFIRMATION`).
     * Taking the deferred out makes a duplicate confirm a clean no-op (mirrors the
     * Rust `take_confirmation`).
     */
    resolve(sessionId: string, approved: boolean): boolean {
        const deferred = this.pending.get(sessionId);
        if (!deferred || deferred.settled()) return false;
        this.pending.delete(sessionId);
        deferred.resolve(approved);
        return true;
    }

    /**
     * Drop any registered deferred for `sessionId` (turn ended), so a stale entry
     * can't mis-route a later confirmation. Idempotent.
     */
    clear(sessionId: string): void {
        this.pending.delete(sessionId);
    }

    /**
     * Resolve every outstanding confirmation as **rejected** (deny).
     *
     * Called when a connection is torn down (close / graceful drain) so any turn
     * parked on a confirmation unparks and finishes cleanly — fail closed (a write is
     * never auto-approved on disconnect) and never leave a turn hung forever.
     */
    rejectAll(): void {
        for (const deferred of this.pending.values()) {
            if (!deferred.settled()) deferred.resolve(false);
        }
        this.pending.clear();
    }
}
