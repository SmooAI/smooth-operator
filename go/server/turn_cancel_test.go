package server

import (
	"context"
	"encoding/json"
	"sync/atomic"
	"testing"
	"time"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/SmooAI/smooth-operator/go/protocol"
)

// User-initiated turn cancellation — the `cancel` action ("Stop button"). The Go port of
// the reference rust/smooth-operator-server/tests/turn_cancel.rs, driven over a real
// WebSocket against a live server. It proves:
//
//  1. Cancel mid-turn stops it. A `cancel` frame while a turn is parked in a tool
//     cancels the turn's context — the tool's in-flight wait is abandoned and its
//     post-wait line never runs — and a terminal `cancelled` event (status 499) is
//     emitted. No eventual_response follows and the connection stays usable.
//  2. Cancel with no active turn is a silent no-op (no event; connection stays live).
//  3. A normal turn still completes with an eventual_response (the cancellation wiring
//     doesn't disturb the happy path).
//  4. Disconnect mid-turn also aborts the turn (no client remains to receive its output).
//
// Plus the single-active-turn rule the same spec mandates (Go-side coverage of what the
// Rust reader loop enforces): a second send_message while a turn is in flight is
// rejected with TURN_IN_PROGRESS rather than run concurrently.
//
// Runs fully offline: a MockLlmProvider scripts a call to a tool that parks the turn on
// a context-aware wait, giving a stable in-flight window to cancel in. No gateway.
//
// Cancellation in Go is COOPERATIVE (a context), not preemptive like dropping a Rust
// future — so the parking tool waits on ctx.Done(), which is what any well-behaved Go
// tool does. That is the one deliberate language-level difference from the reference.

const cancelSlowTool = "slow_probe"

// slowToolProbe records what a parked tool observed: that it started, whether its
// context was cancelled out from under it (the positive signal that the turn was
// abandoned mid-wait), and whether it ever reached its post-wait completion.
type slowToolProbe struct {
	started   chan struct{}
	cancelled atomic.Bool
	finished  atomic.Bool
	startOnce atomic.Bool
}

func newSlowToolProbe() *slowToolProbe {
	return &slowToolProbe{started: make(chan struct{})}
}

// tool builds the parking tool: it signals that it started, then waits far longer than
// any test. A cancelled turn cancels its ctx, so it returns early with cancelled=true
// and never sets finished.
func (p *slowToolProbe) tool() core.Tool {
	return core.FuncTool{
		ToolName: cancelSlowTool,
		Desc:     "parks the turn for cancellation tests",
		Params:   map[string]any{"type": "object"},
		Fn: func(ctx context.Context, _ map[string]any) (string, error) {
			if p.startOnce.CompareAndSwap(false, true) {
				close(p.started)
			}
			select {
			case <-ctx.Done():
				p.cancelled.Store(true)
				return "", ctx.Err()
			case <-time.After(time.Hour):
				// Only reached if the turn was NOT cancelled.
				p.finished.Store(true)
				return "done", nil
			}
		},
	}
}

// awaitStart blocks until the turn is provably parked inside the tool.
func (p *slowToolProbe) awaitStart(t *testing.T) {
	t.Helper()
	select {
	case <-p.started:
	case <-time.After(5 * time.Second):
		t.Fatal("turn never parked in the slow tool")
	}
}

// awaitCancelled polls until the parked tool observed its context cancellation.
func (p *slowToolProbe) awaitCancelled(t *testing.T) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if p.cancelled.Load() {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("timed out waiting for the turn's context to be cancelled")
}

// slowToolServer spins up a server whose mock scripts a single call to the parking tool
// (so the turn parks and never returns on its own), then a text reply that a
// non-cancelled turn would settle with.
func slowToolServer(t *testing.T, probe *slowToolProbe) *LocalServer {
	t.Helper()
	mock := core.NewMockLlmProvider()
	mock.PushToolCall("call-1", cancelSlowTool, `{}`)
	mock.PushText("Finished the slow thing.")

	ls, err := SpawnLocal(
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(mock),
		WithLocalServerOption(WithTools([]core.Tool{probe.tool()})),
	)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	t.Cleanup(func() { _ = ls.Shutdown() })
	return ls
}

// nextRawEv reads the next server event WITHOUT the pong/keepalive filtering nextEv
// applies — the no-op test asserts that a pong is literally the next thing on the wire.
func nextRawEv(t *testing.T, transport protocol.Transport, within time.Duration) map[string]any {
	t.Helper()
	select {
	case data, ok := <-transport.Receive():
		if !ok {
			t.Fatalf("transport closed before expected event")
		}
		var ev map[string]any
		if err := json.Unmarshal(data, &ev); err != nil {
			t.Fatalf("decode event: %v (raw=%s)", err, data)
		}
		return ev
	case <-time.After(within):
		t.Fatalf("timed out waiting for next event")
		return nil
	}
}

// recvWithin returns the next event, or nil if none arrives within the window (used to
// assert that NOTHING follows a cancellation).
func recvWithin(t *testing.T, transport protocol.Transport, within time.Duration) map[string]any {
	t.Helper()
	select {
	case data, ok := <-transport.Receive():
		if !ok {
			return nil // transport closed — no event
		}
		var ev map[string]any
		if err := json.Unmarshal(data, &ev); err != nil {
			t.Fatalf("decode event: %v (raw=%s)", err, data)
		}
		return ev
	case <-time.After(within):
		return nil
	}
}

// recvUntil reads events until one of type typ arrives, collecting the ones it skipped
// (the Go analog of the Rust common::recv_until).
func recvUntil(t *testing.T, transport protocol.Transport, typ string, within time.Duration) (map[string]any, []map[string]any) {
	t.Helper()
	var seen []map[string]any
	deadline := time.After(within)
	for {
		select {
		case data, ok := <-transport.Receive():
			if !ok {
				t.Fatalf("transport closed waiting for %q (seen=%d events)", typ, len(seen))
			}
			var ev map[string]any
			if err := json.Unmarshal(data, &ev); err != nil {
				t.Fatalf("decode event: %v (raw=%s)", err, data)
			}
			if got, _ := ev["type"].(string); got == typ {
				return ev, seen
			}
			seen = append(seen, ev)
		case <-deadline:
			t.Fatalf("timed out waiting for %q (seen=%d events)", typ, len(seen))
			return nil, nil
		}
	}
}

// TestCancelMidTurnAbortsAndEmitsCancelled — a cancel while the turn is parked in a tool
// aborts the turn and emits the terminal `cancelled` event; nothing follows it, and the
// connection stays usable.
func TestCancelMidTurnAbortsAndEmitsCancelled(t *testing.T) {
	probe := newSlowToolProbe()
	ls := slowToolServer(t, probe)
	transport := connectTransport(t, ls)
	defer func() { _ = transport.Close() }()

	sessionID := createSession(t, transport)

	// Start a turn; it parks inside the slow tool.
	sendFrame(t, transport, map[string]any{
		"action":    "send_message",
		"requestId": "turn-1",
		"sessionId": sessionID,
		"message":   "please do the slow thing",
	})
	probe.awaitStart(t)
	if probe.finished.Load() {
		t.Fatal("tool must not have finished yet")
	}

	// Cancel it, reusing the turn's requestId (the correlation convention).
	sendFrame(t, transport, map[string]any{"action": "cancel", "requestId": "turn-1"})

	// A terminal `cancelled` event arrives, echoing the turn's requestId. (Skip the ack /
	// stream events that were already in flight when the cancel landed.)
	ev, _ := recvUntil(t, transport, "cancelled", 5*time.Second)
	if rid, _ := ev["requestId"].(string); rid != "turn-1" {
		t.Fatalf("cancelled requestId = %v, want turn-1 (event=%s)", ev["requestId"], mustJSON(ev))
	}
	if status, _ := asInt(ev["status"]); status != 499 {
		t.Fatalf("cancelled status = %v, want 499 (event=%s)", ev["status"], mustJSON(ev))
	}
	if rid, ok := dot(t, ev, "data.requestId"); !ok || rid != "turn-1" {
		t.Fatalf("cancelled data.requestId = %v, want turn-1 (event=%s)", rid, mustJSON(ev))
	}
	if status, ok := dot(t, ev, "data.status"); !ok {
		t.Fatalf("cancelled data.status missing (event=%s)", mustJSON(ev))
	} else if n, _ := asInt(status); n != 499 {
		t.Fatalf("cancelled data.status = %v, want 499 (event=%s)", status, mustJSON(ev))
	}
	// No answer payload: a cancelled turn produced no assistant message.
	if _, ok := dot(t, ev, "data.messageId"); ok {
		t.Fatalf("cancelled must carry no messageId (event=%s)", mustJSON(ev))
	}

	// The turn was abandoned mid-wait: the tool saw its context cancelled and never
	// reached its post-wait completion.
	probe.awaitCancelled(t)
	if probe.finished.Load() {
		t.Fatal("cancelled turn's tool must never reach its post-wait completion")
	}

	// No further terminal event (no eventual_response) follows the cancellation.
	if after := recvWithin(t, transport, 500*time.Millisecond); after != nil {
		t.Fatalf("no event should follow the cancellation, got: %s", mustJSON(after))
	}

	// The connection is still alive and usable.
	sendFrame(t, transport, map[string]any{"action": "ping", "requestId": "p1"})
	pong := nextRawEv(t, transport, 5*time.Second)
	if typ, _ := pong["type"].(string); typ != "pong" {
		t.Fatalf("expected pong after cancellation, got %s", mustJSON(pong))
	}
	if rid, _ := pong["requestId"].(string); rid != "p1" {
		t.Fatalf("pong requestId = %v, want p1", pong["requestId"])
	}
}

// TestCancelWithNoActiveTurnIsNoop — a cancel with nothing running emits nothing and
// leaves the connection live.
func TestCancelWithNoActiveTurnIsNoop(t *testing.T) {
	mock := core.NewMockLlmProvider()
	mock.PushText("hi")
	ls, err := SpawnLocal(WithLocalAddr("127.0.0.1:0"), WithLocalChatClient(mock))
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	t.Cleanup(func() { _ = ls.Shutdown() })

	transport := connectTransport(t, ls)
	defer func() { _ = transport.Close() }()
	_ = createSession(t, transport)

	// Cancel with nothing running: must emit nothing.
	sendFrame(t, transport, map[string]any{"action": "cancel", "requestId": "nope"})

	// So the NEXT event on the wire is the pong (the cancel produced no event of its own).
	sendFrame(t, transport, map[string]any{"action": "ping", "requestId": "p1"})
	ev := nextRawEv(t, transport, 5*time.Second)
	if typ, _ := ev["type"].(string); typ != "pong" {
		t.Fatalf("cancel must not emit an event; got: %s", mustJSON(ev))
	}
	if rid, _ := ev["requestId"].(string); rid != "p1" {
		t.Fatalf("pong requestId = %v, want p1", ev["requestId"])
	}
}

// TestNormalTurnStillCompletes — the cancellation wiring doesn't disturb the happy path.
func TestNormalTurnStillCompletes(t *testing.T) {
	mock := core.NewMockLlmProvider()
	mock.PushText("All done here.")
	ls, err := SpawnLocal(WithLocalAddr("127.0.0.1:0"), WithLocalChatClient(mock))
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	t.Cleanup(func() { _ = ls.Shutdown() })

	transport := connectTransport(t, ls)
	defer func() { _ = transport.Close() }()
	sessionID := createSession(t, transport)

	sendFrame(t, transport, map[string]any{
		"action":    "send_message",
		"requestId": "turn-ok",
		"sessionId": sessionID,
		"message":   "hello",
	})

	done, seen := recvUntil(t, transport, "eventual_response", 10*time.Second)
	if rid, _ := done["requestId"].(string); rid != "turn-ok" {
		t.Fatalf("eventual_response requestId = %v, want turn-ok (event=%s)", done["requestId"], mustJSON(done))
	}
	if status, _ := asInt(done["status"]); status != 200 {
		t.Fatalf("eventual_response status = %v, want 200", done["status"])
	}
	for _, ev := range seen {
		if typ, _ := ev["type"].(string); typ == "cancelled" {
			t.Fatalf("a normal turn must not emit a cancelled event: %s", mustJSON(ev))
		}
	}
}

// TestDisconnectMidTurnAbortsTheTurn — the client hanging up mid-turn aborts the turn
// (no client remains to receive its output).
func TestDisconnectMidTurnAbortsTheTurn(t *testing.T) {
	probe := newSlowToolProbe()
	ls := slowToolServer(t, probe)
	transport := connectTransport(t, ls)

	sessionID := createSession(t, transport)
	sendFrame(t, transport, map[string]any{
		"action":    "send_message",
		"requestId": "turn-x",
		"sessionId": sessionID,
		"message":   "please do the slow thing",
	})
	probe.awaitStart(t)

	// Client hangs up mid-turn.
	if err := transport.Close(); err != nil {
		t.Fatalf("close transport: %v", err)
	}

	// The server aborts the in-flight turn: the tool's context is cancelled and it never
	// reaches its post-wait completion.
	probe.awaitCancelled(t)
	if probe.finished.Load() {
		t.Fatal("disconnect must abort the turn before it completes")
	}
}

// TestSecondSendMessageWhileTurnInFlightIsRejected — ONE active turn per connection: a
// second send_message while one is running is rejected with TURN_IN_PROGRESS, never run
// concurrently.
func TestSecondSendMessageWhileTurnInFlightIsRejected(t *testing.T) {
	probe := newSlowToolProbe()
	ls := slowToolServer(t, probe)
	transport := connectTransport(t, ls)
	defer func() { _ = transport.Close() }()

	sessionID := createSession(t, transport)
	sendFrame(t, transport, map[string]any{
		"action":    "send_message",
		"requestId": "turn-1",
		"sessionId": sessionID,
		"message":   "please do the slow thing",
	})
	probe.awaitStart(t)

	// A second turn on the same socket while the first is parked.
	sendFrame(t, transport, map[string]any{
		"action":    "send_message",
		"requestId": "turn-2",
		"sessionId": sessionID,
		"message":   "and another thing",
	})

	ev, _ := recvUntil(t, transport, "error", 5*time.Second)
	if rid, _ := ev["requestId"].(string); rid != "turn-2" {
		t.Fatalf("error requestId = %v, want turn-2 (event=%s)", ev["requestId"], mustJSON(ev))
	}
	code, ok := dot(t, ev, "error.code")
	if !ok || code != "TURN_IN_PROGRESS" {
		t.Fatalf("error code = %v, want TURN_IN_PROGRESS (event=%s)", code, mustJSON(ev))
	}

	// The rejected frame started no turn: cancelling clears the ONE turn that is running,
	// and the connection settles with a single `cancelled`.
	sendFrame(t, transport, map[string]any{"action": "cancel", "requestId": "turn-1"})
	got, _ := recvUntil(t, transport, "cancelled", 5*time.Second)
	if rid, _ := got["requestId"].(string); rid != "turn-1" {
		t.Fatalf("cancelled requestId = %v, want turn-1 (event=%s)", got["requestId"], mustJSON(got))
	}
}
