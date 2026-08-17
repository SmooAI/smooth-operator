package server

import (
	"context"
	"sync"
)

// Target is a delivery target: one connection, or every connection for a session /
// user / org / agent. A comparable struct, so it is a map key by value. Ports the
// Rust reference's Target.
type Target struct {
	Kind string
	ID   string
}

// TargetKinds are the five kinds a publish may name. Anything else is the caller's
// error.
var TargetKinds = []string{"connection", "session", "user", "org", "agent"}

// Backplane is the connection-registry seam: every connection attaches its outbound
// sink under a connection id, associates the targets it belongs to, and detaches when
// its read loop exits. Publishing to a target reaches every connection for it — the
// plug point for non-AI publishers (job status, ingestion progress, notifications)
// that need a connected client without going through an agent turn. The Go analog of
// the Rust Backplane trait, including its 5-target fan-out. The bundled
// InMemoryBackplane is single-process; Redis/NATS impls satisfy the same interface and
// additionally reach other pods' sockets.
type Backplane interface {
	// Attach registers a connection's outbound sink. sink delivers an already-built
	// event frame to the connection's writer. The connection is always reachable as
	// Target{"connection", connID}.
	Attach(ctx context.Context, connID string, sink func(event map[string]any))
	// Associate links a connection to a target so a publish to it lands here.
	// Idempotent — the session chokepoint runs on every sessionId-bearing frame, so a
	// re-association must not double-count a delivery.
	Associate(ctx context.Context, connID string, target Target)
	// Publish fans an event out to every connection associated with target, returning
	// how many sinks it reached. The count is what `POST /admin/publish` reports as
	// `delivered`, so it must never claim a delivery that did not happen.
	Publish(ctx context.Context, target Target, event map[string]any) int
	// Detach removes a connection's sink and every association to it. Always run on
	// connection teardown: a leaked association resolves to a dead socket and would
	// inflate `delivered` forever.
	Detach(ctx context.Context, connID string)
}

// InMemoryBackplane is a single-process Backplane: connection sinks plus a
// target→connections index, so all five target kinds resolve locally. No cross-POD
// fan-out (that's the Redis/NATS seam). Safe for concurrent use.
type InMemoryBackplane struct {
	// ponytail: one mutex over the whole registry. Attach/detach run once per
	// connection and publish is two map lookups, so contention is not the bottleneck —
	// shard per target only if a profile ever says otherwise.
	mu    sync.Mutex
	sinks map[string]func(event map[string]any)
	// targetConns indexes target → conn ids for publish fan-out; connTargets is the
	// reverse, so Detach tears every association down without scanning all targets.
	targetConns map[Target]map[string]struct{}
	connTargets map[string]map[Target]struct{}
}

// NewInMemoryBackplane returns an empty in-memory backplane.
func NewInMemoryBackplane() *InMemoryBackplane {
	return &InMemoryBackplane{
		sinks:       map[string]func(event map[string]any){},
		targetConns: map[Target]map[string]struct{}{},
		connTargets: map[string]map[Target]struct{}{},
	}
}

// Attach registers connID's sink and makes it reachable by its own connection id.
func (b *InMemoryBackplane) Attach(_ context.Context, connID string, sink func(event map[string]any)) {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.sinks[connID] = sink
	b.link(connID, Target{Kind: "connection", ID: connID})
}

// Associate links connID to target.
func (b *InMemoryBackplane) Associate(_ context.Context, connID string, target Target) {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.link(connID, target)
}

// Publish delivers event to every connection associated with target.
func (b *InMemoryBackplane) Publish(_ context.Context, target Target, event map[string]any) int {
	b.mu.Lock()
	// Snapshot under the lock, invoke OUTSIDE it: a host's sink is arbitrary code, and
	// one bad one held under the registry lock would block every attach, detach and
	// publish in the process.
	sinks := make([]func(event map[string]any), 0, len(b.targetConns[target]))
	for connID := range b.targetConns[target] {
		if sink := b.sinks[connID]; sink != nil {
			sinks = append(sinks, sink)
		}
	}
	b.mu.Unlock()
	for _, sink := range sinks {
		sink(event)
	}
	return len(sinks)
}

// Detach removes connID's sink and every association to it.
func (b *InMemoryBackplane) Detach(_ context.Context, connID string) {
	b.mu.Lock()
	defer b.mu.Unlock()
	delete(b.sinks, connID)
	for target := range b.connTargets[connID] {
		conns := b.targetConns[target]
		delete(conns, connID)
		if len(conns) == 0 {
			delete(b.targetConns, target)
		}
	}
	delete(b.connTargets, connID)
}

// link records both directions of a conn↔target association. Caller holds b.mu.
func (b *InMemoryBackplane) link(connID string, target Target) {
	if b.targetConns[target] == nil {
		b.targetConns[target] = map[string]struct{}{}
	}
	b.targetConns[target][connID] = struct{}{}
	if b.connTargets[connID] == nil {
		b.connTargets[connID] = map[Target]struct{}{}
	}
	b.connTargets[connID][target] = struct{}{}
}

// IsAttached reports whether connID currently has a sink (used by tests to verify
// detach-after-loop ran).
func (b *InMemoryBackplane) IsAttached(connID string) bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	_, ok := b.sinks[connID]
	return ok
}
