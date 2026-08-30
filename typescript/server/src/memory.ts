/**
 * Durable auto-recall — the server side of the engine's `Memory` seam (Rust PR #330).
 *
 * The engine already knows how to auto-recall: give `AgentOptions.memory` a store and it pulls the
 * entries relevant to the user's message into the turn's context. What was missing on this server is
 * the way for a HOST to say *which* store — so every turn ran without auto-recall regardless of what
 * the deployment had.
 *
 * Mirrors the Rust `StorageAdapter::memory_for_access` seam. `access` is threaded (as it is for
 * knowledge) so a multi-tenant backend can bind memory to the requester's org/user; a single-tenant
 * host — Big Smooth's daemon, the reason this seam exists — ignores it and returns its one store.
 */
import type { Memory } from '@smooai/smooth-operator-core';

import type { AccessContext } from './auth.js';

/** Supplies the durable-recall handle for a turn. */
export interface MemoryProvider {
    /**
     * The memory to auto-recall from for a caller with this access, or `undefined` for none.
     * `undefined` is the default for every deployment that has not opted in, and leaves the turn
     * byte-for-byte unchanged.
     */
    memoryForAccess(access: AccessContext): Memory | undefined;
}

/**
 * A {@link MemoryProvider} over one unscoped store — the single-tenant case (Big Smooth's daemon
 * hands its SQLite-backed store straight through). A multi-tenant host implements the interface
 * itself and keys off `access` instead.
 */
export class StaticMemoryProvider implements MemoryProvider {
    constructor(private readonly memory: Memory | undefined) {}

    memoryForAccess(_access: AccessContext): Memory | undefined {
        return this.memory;
    }
}
