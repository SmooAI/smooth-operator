package server

// Backplane fan-out — ports the Rust reference's backplane.rs unit tests. The point is
// that a routed target ACTUALLY RECEIVES: a registry that accepts an association but
// delivers nothing would pass a shape-only test while being useless, and `delivered`
// would be a lying 0.

import (
	"context"
	"testing"
)

func collector(n int) (func(map[string]any), chan map[string]any) {
	ch := make(chan map[string]any, n)
	return func(e map[string]any) { ch <- e }, ch
}

func TestBackplanePublishesToASessionAcrossItsConnections(t *testing.T) {
	ctx := context.Background()
	bp := NewInMemoryBackplane()
	sinkA, gotA := collector(1)
	sinkB, gotB := collector(1)
	bp.Attach(ctx, "conn-a", sinkA)
	bp.Attach(ctx, "conn-b", sinkB)
	bp.Associate(ctx, "conn-a", Target{Kind: "session", ID: "s1"})
	bp.Associate(ctx, "conn-b", Target{Kind: "session", ID: "s1"})

	if n := bp.Publish(ctx, Target{Kind: "session", ID: "s1"}, map[string]any{"hi": 1}); n != 2 {
		t.Fatalf("delivered = %d, want 2", n)
	}
	if len(gotA) != 1 || len(gotB) != 1 {
		t.Errorf("both sinks must receive: a=%d b=%d", len(gotA), len(gotB))
	}
}

func TestBackplaneUnknownTargetDeliversToNobody(t *testing.T) {
	bp := NewInMemoryBackplane()
	if n := bp.Publish(context.Background(), Target{Kind: "session", ID: "nope"}, map[string]any{}); n != 0 {
		t.Errorf("delivered = %d, want 0", n)
	}
}

func TestBackplaneDetachRemovesEveryAssociation(t *testing.T) {
	ctx := context.Background()
	bp := NewInMemoryBackplane()
	sink, _ := collector(1)
	bp.Attach(ctx, "conn-x", sink)
	bp.Associate(ctx, "conn-x", Target{Kind: "user", ID: "u1"})

	bp.Detach(ctx, "conn-x")
	if bp.IsAttached("conn-x") {
		t.Error("sink survived detach")
	}
	// A leaked association would resolve to a dead socket and inflate `delivered` forever.
	if n := bp.Publish(ctx, Target{Kind: "user", ID: "u1"}, map[string]any{}); n != 0 {
		t.Errorf("user target after detach = %d, want 0", n)
	}
	if n := bp.Publish(ctx, Target{Kind: "connection", ID: "conn-x"}, map[string]any{}); n != 0 {
		t.Errorf("connection target after detach = %d, want 0", n)
	}
}

func TestBackplaneAConnectionCanServeMultipleTargets(t *testing.T) {
	ctx := context.Background()
	bp := NewInMemoryBackplane()
	sink, got := collector(2)
	bp.Attach(ctx, "c", sink)
	bp.Associate(ctx, "c", Target{Kind: "session", ID: "s"})
	bp.Associate(ctx, "c", Target{Kind: "org", ID: "o"})

	if n := bp.Publish(ctx, Target{Kind: "org", ID: "o"}, map[string]any{"e": "org"}); n != 1 {
		t.Errorf("org = %d, want 1", n)
	}
	if n := bp.Publish(ctx, Target{Kind: "session", ID: "s"}, map[string]any{"e": "sess"}); n != 1 {
		t.Errorf("session = %d, want 1", n)
	}
	if len(got) != 2 {
		t.Errorf("both events must land: %d", len(got))
	}
}

func TestBackplaneAssociateIsIdempotent(t *testing.T) {
	// scopedSession associates on EVERY sessionId-bearing frame, so this path is hot: a
	// re-association must not double-count the delivery.
	ctx := context.Background()
	bp := NewInMemoryBackplane()
	sink, got := collector(2)
	bp.Attach(ctx, "c", sink)
	bp.Associate(ctx, "c", Target{Kind: "session", ID: "s"})
	bp.Associate(ctx, "c", Target{Kind: "session", ID: "s"})

	if n := bp.Publish(ctx, Target{Kind: "session", ID: "s"}, map[string]any{}); n != 1 {
		t.Errorf("delivered = %d, want 1", n)
	}
	if len(got) != 1 {
		t.Errorf("sink received %d events, want 1", len(got))
	}
}

func TestBackplaneAttachAloneMakesTheConnectionReachable(t *testing.T) {
	ctx := context.Background()
	bp := NewInMemoryBackplane()
	sink, got := collector(1)
	bp.Attach(ctx, "c", sink)

	if n := bp.Publish(ctx, Target{Kind: "connection", ID: "c"}, map[string]any{"ping": true}); n != 1 {
		t.Fatalf("delivered = %d, want 1", n)
	}
	if len(got) != 1 {
		t.Error("event never reached the sink")
	}
}
