package server

import (
	"context"
	"sync/atomic"
	"testing"
	"time"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// A `cancelled` event has to be the LAST thing the client sees for that requestId.
//
// turn_cancel_test.go covers the ordinary path, where the runner's event loop checks
// ctx.Err() per event and stops. This file covers the paths that never reach that loop:
// a Rich Interaction raise tool calls sink() straight from inside Execute, on the
// engine's goroutine, with no ctx check of its own — so an interaction_required and a
// toolCall chunk could land AFTER the cancellation. The fix gates the turn's sink once,
// which is the choke point every emit path goes through.
//
// The race is made deterministic rather than slept on: the model script asks for TWO
// tools in ONE response, and the engine dispatches them back-to-back with no ctx check
// in between. The first blocks until the test has cancelled the turn AND seen the
// terminal event; the second is the raise tool, which then emits into a turn the client
// already believes is over.

const cancelGateTool = "cancel_gate"

// cancelGate blocks the engine's tool loop until the test releases it, so the test
// controls exactly when the raise tool that follows gets to emit.
type cancelGate struct {
	started   chan struct{}
	release   chan struct{}
	startOnce atomic.Bool
}

func newCancelGate() *cancelGate {
	return &cancelGate{started: make(chan struct{}), release: make(chan struct{})}
}

func (g *cancelGate) tool() core.Tool {
	return core.FuncTool{
		ToolName: cancelGateTool,
		Desc:     "holds the tool loop open for cancellation tests",
		Params:   map[string]any{"type": "object"},
		Fn: func(_ context.Context, _ map[string]any) (string, error) {
			if g.startOnce.CompareAndSwap(false, true) {
				close(g.started)
			}
			// Deliberately NOT ctx-aware: this stands in for a host tool that finishes
			// its work despite the cancellation. The point of the test is what happens
			// to the SINK afterwards, not whether this tool cooperates.
			select {
			case <-g.release:
				return "gate open", nil
			case <-time.After(10 * time.Second):
				return "", context.DeadlineExceeded
			}
		},
	}
}

func (g *cancelGate) awaitStart(t *testing.T) {
	t.Helper()
	select {
	case <-g.started:
	case <-time.After(5 * time.Second):
		t.Fatal("the gate tool never ran — the turn never reached the tool loop")
	}
}

// TestNothingIsEmittedAfterCancelled proves the sink gate: once `cancelled` is on the
// wire, a raise tool running afterwards emits nothing.
func TestNothingIsEmittedAfterCancelled(t *testing.T) {
	gate := newCancelGate()

	mock := core.NewMockLlmProvider()
	// ONE response, TWO calls: the engine runs them serially with no cancellation
	// checkpoint between, so the raise tool is guaranteed to run after the cancel.
	mock.PushResponse(core.ChatResponse{ToolCalls: []core.ToolCall{
		{ID: "call-gate", Name: cancelGateTool, Arguments: `{}`},
		{ID: "call-raise", Name: "request_choices", Arguments: choicesQuestionsArg},
	}})
	mock.PushText("Never reached.")

	ls, err := SpawnLocal(
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(mock),
		WithLocalServerOption(WithTools([]core.Tool{gate.tool()})),
	)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer func() { _ = ls.Shutdown() }()

	transport := connectTransport(t, ls)
	defer transport.Close()

	sessionID := createSessionSupports(t, transport, []string{"choice_chips"})
	sendFrame(t, transport, map[string]any{
		"action":    "send_message",
		"requestId": "r-msg",
		"sessionId": sessionID,
		"message":   "I want to sign up",
	})

	// The turn is provably inside the tool loop.
	gate.awaitStart(t)

	sendFrame(t, transport, map[string]any{"action": "cancel", "requestId": "r-msg"})

	cancelled, _ := recvUntil(t, transport, "cancelled", 5*time.Second)
	if status := mustStatus(t, cancelled); status != 499 {
		t.Fatalf("cancelled status = %d, want 499 (event=%s)", status, mustJSON(cancelled))
	}

	// Only now let the raise tool run. Everything it emits happens strictly after the
	// client was told the turn ended.
	close(gate.release)

	if ev := recvWithin(t, transport, 2*time.Second); ev != nil {
		typ, _ := ev["type"].(string)
		t.Fatalf("cancelled must be terminal, but a %q arrived after it: %s", typ, mustJSON(ev))
	}
}

// TestOtpIsNotDispatchedAfterCancel — the worst post-cancel side effect is not a stray
// frame, it is a real verification code sent to a real phone or inbox after the visitor
// hit Stop.
//
// offerOtp used to run on the CONNECTION's context, not the turn's, so cancelling the
// turn did nothing to it; and the turn tail's ctx check sits several statements earlier,
// with awaits in between. The guard therefore lives immediately before the dispatch. A
// host OtpService is under no obligation to honor the context — fakeOtp deliberately
// ignores it, which is exactly the case being defended.
func TestOtpIsNotDispatchedAfterCancel(t *testing.T) {
	svc := &fakeOtp{delivery: OtpDelivery{Channel: OtpChannelEmail, MaskedDestination: "a***@example.com"}}
	d := NewFrameDispatcher(NewInMemorySessionStore(), nil, AccessContext{}, "", nil, nil, nil, nil, nil, "", nil, nil, svc, nil)

	var emitted []map[string]any
	sink := func(ev map[string]any) { emitted = append(emitted, ev) }

	ctx, cancel := context.WithCancel(context.Background())
	cancel() // the turn was cancelled before the tail got here

	d.offerOtp(ctx, "sess-1", "lookup_orders", OtpContact{Email: "a@example.com"}, "r-msg", sink)

	if svc.sentSession != "" {
		t.Fatalf("a code was sent to %q after the turn was cancelled", svc.sentContact.Email)
	}
	for _, ev := range emitted {
		if typ, _ := ev["type"].(string); typ == "otp_sent" {
			t.Fatalf("otp_sent emitted for a cancelled turn: %s", mustJSON(ev))
		}
	}
}
