package server

import (
	"context"
	"sync"
)

// Backplane is the connection-registry seam: every connection attaches its outbound
// sink under a connection id so events published from anywhere (this process, or —
// with a Redis/NATS impl — another pod) can reach it, and detaches when its read
// loop exits. The Go analog of the Rust Backplane trait. The bundled
// InMemoryBackplane is the single-process reference impl (no cross-pod fan-out);
// Redis/NATS impls satisfy the same interface — that seam is left open for MVP.
type Backplane interface {
	// Attach registers a connection's outbound sink. sink delivers an already-built
	// event frame to the connection's writer.
	Attach(ctx context.Context, connID string, sink func(event map[string]any))
	// Publish fans an event out to a connection's attached sink, if present.
	Publish(ctx context.Context, connID string, event map[string]any)
	// Detach removes a connection's sink. Always run on connection teardown.
	Detach(ctx context.Context, connID string)
}

// InMemoryBackplane is a single-process Backplane: a connID→sink map. No cross-pod
// fan-out (that's the Redis/NATS seam). Safe for concurrent use.
type InMemoryBackplane struct {
	mu    sync.Mutex
	sinks map[string]func(event map[string]any)
}

// NewInMemoryBackplane returns an empty in-memory backplane.
func NewInMemoryBackplane() *InMemoryBackplane {
	return &InMemoryBackplane{sinks: map[string]func(event map[string]any){}}
}

// Attach registers connID's sink.
func (b *InMemoryBackplane) Attach(_ context.Context, connID string, sink func(event map[string]any)) {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.sinks[connID] = sink
}

// Publish delivers event to connID's sink if it is still attached.
func (b *InMemoryBackplane) Publish(_ context.Context, connID string, event map[string]any) {
	b.mu.Lock()
	sink := b.sinks[connID]
	b.mu.Unlock()
	if sink != nil {
		sink(event)
	}
}

// Detach removes connID's sink.
func (b *InMemoryBackplane) Detach(_ context.Context, connID string) {
	b.mu.Lock()
	defer b.mu.Unlock()
	delete(b.sinks, connID)
}

// IsAttached reports whether connID currently has a sink (used by tests to verify
// detach-after-loop ran).
func (b *InMemoryBackplane) IsAttached(connID string) bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	_, ok := b.sinks[connID]
	return ok
}
