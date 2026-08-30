package server

import (
	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// Durable auto-recall — the server side of the engine's Memory seam (Rust PR #330).
//
// The engine already knows how to auto-recall: give AgentOptions.Memory a store and it pulls the
// entries relevant to the user's message into the turn's context. What was missing on this server
// is the way for a HOST to say WHICH store — so every turn ran without auto-recall regardless of
// what the deployment had.
//
// Mirrors the Rust StorageAdapter::memory_for_access seam. access is threaded (as it is for
// knowledge) so a multi-tenant backend can bind memory to the requester's org/user; a single-tenant
// host — Big Smooth's daemon, the reason this seam exists — ignores it and returns its one store.

// MemoryProvider supplies the durable-recall handle for a turn.
type MemoryProvider interface {
	// MemoryForAccess returns the memory to auto-recall from for a caller with this access, or nil
	// for none. nil is the default for every deployment that has not opted in, and leaves the turn
	// byte-for-byte unchanged.
	MemoryForAccess(access AccessContext) core.Memory
}

// StaticMemoryProvider is a MemoryProvider over one unscoped store — the single-tenant case (Big
// Smooth's daemon hands its SQLite-backed store straight through). A multi-tenant host implements
// the interface itself and keys off access instead.
type StaticMemoryProvider struct {
	memory core.Memory
}

// NewStaticMemoryProvider wraps one store (nil disables auto-recall).
func NewStaticMemoryProvider(memory core.Memory) *StaticMemoryProvider {
	return &StaticMemoryProvider{memory: memory}
}

// MemoryForAccess implements MemoryProvider.
func (p *StaticMemoryProvider) MemoryForAccess(_ AccessContext) core.Memory { return p.memory }
