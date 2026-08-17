package server

import (
	"context"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// fakeExecutor is a stand-in durable AgentExecutor: it records whether it was
// driven, so a test can prove the selection actually routed the turn to it (not
// just returned it). It never runs a real turn.
type fakeExecutor struct {
	used bool
}

func (f *fakeExecutor) Execute(_ context.Context, _ *core.SmoothAgent, _ string, _ []core.ChatMessage) (core.AgentRunResponse, error) {
	f.used = true
	return core.AgentRunResponse{}, nil
}

func (f *fakeExecutor) ExecuteStreaming(_ context.Context, _ *core.SmoothAgent, _ string, _ *core.SmoothAgentThread) (*core.Stream, error) {
	f.used = true
	return nil, nil
}

// With durable mode opted in AND an executor injected, the injected durable
// executor is handed back verbatim — the SAME instance the turn will drive. This
// is the whole point of the injected slot: a durable backend built outside this
// binary has to survive the trip through turnExecutor.
func TestTurnExecutor_DurableOnUsesInjected(t *testing.T) {
	fake := &fakeExecutor{}
	for _, on := range []string{"1", "true", "TRUE", " on ", "yes"} {
		selected := turnExecutor(fake, on)
		if selected != core.AgentExecutor(fake) {
			t.Fatalf("env=%q: expected injected durable executor to be used, got %T", on, selected)
		}
	}
}

// Durable mode off (unset/empty/unrecognized) ignores the injected slot and runs
// in-process, even when a durable executor is present — getting this backwards
// would silently make every deployed turn durable.
func TestTurnExecutor_DurableOffFallsBackToInProcess(t *testing.T) {
	fake := &fakeExecutor{}
	for _, off := range []string{"", " ", "0", "false", "off", "no", "maybe"} {
		selected := turnExecutor(fake, off)
		if selected == core.AgentExecutor(fake) {
			t.Fatalf("env=%q: durable is opt-in only; injected executor must NOT be used", off)
		}
		if _, ok := selected.(*core.InProcessExecutor); !ok {
			t.Fatalf("env=%q: expected *core.InProcessExecutor, got %T", off, selected)
		}
	}
}

// Durable requested but nothing injected ⇒ a fresh in-process executor (warn +
// fall back), never a nil that would panic the turn.
func TestTurnExecutor_DurableRequestedButNothingInjected(t *testing.T) {
	selected := turnExecutor(nil, "1")
	if _, ok := selected.(*core.InProcessExecutor); !ok {
		t.Fatalf("expected fallback *core.InProcessExecutor, got %T", selected)
	}
}

// Nothing injected, durable off ⇒ a non-nil in-process executor. (Unlike Rust's
// Arc, Go pointers to the zero-size InProcessExecutor may compare equal, so the
// invariant here is the concrete type, not distinct identity.)
func TestTurnExecutor_NoInjectionBuildsInProcess(t *testing.T) {
	selected := turnExecutor(nil, "")
	if selected == nil {
		t.Fatal("expected a non-nil in-process executor")
	}
	if _, ok := selected.(*core.InProcessExecutor); !ok {
		t.Fatalf("expected *core.InProcessExecutor, got %T", selected)
	}
}

// The opt-in parse matches the Rust reference exactly.
func TestDurableRequested(t *testing.T) {
	for _, on := range []string{"1", "true", "TRUE", " on ", "yes", "Yes", "  1  "} {
		if !durableRequested(on) {
			t.Errorf("%q should opt in", on)
		}
	}
	for _, off := range []string{"", " ", "0", "false", "off", "no", "maybe", "enabled"} {
		if durableRequested(off) {
			t.Errorf("%q should stay off", off)
		}
	}
}
