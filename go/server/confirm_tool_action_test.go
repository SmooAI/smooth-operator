package server

import (
	"context"
	"encoding/json"
	"testing"
	"time"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/SmooAI/smooth-operator/go/protocol"
)

// Write-confirmation HITL — the pause → confirm_tool_action → resume path, driven over
// the real WebSocket transport against a live server (the Go port of the Rust
// rust/smooth-operator-server/tests/confirm_tool_action.rs).
//
// The conformance scenario covers the APPROVE path; these tests additionally cover the
// REJECT path (the gated tool is blocked, the model sees a denial, the turn still
// completes — no hang) and the duplicate-confirm no-op, matching the Rust reference.
//
// Runs fully offline: a MockLlmProvider scripts the gated delete_record call so there is
// no gateway. delete_record is gated via WithConfirmTools, exercising the full
// pause/resume seam.

const (
	confirmGatedTool = "delete_record"
)

// hitlServer spins up a local server whose delete_record tool is gated behind HITL and
// whose mock LLM scripts: a delete_record call, then a final text reply.
func hitlServer(t *testing.T) *LocalServer {
	t.Helper()
	mock := core.NewMockLlmProvider()
	mock.PushToolCall("call-1", confirmGatedTool, `{"id":"42"}`)
	mock.PushText("Done — record 42 was deleted.")

	tools := []core.Tool{core.FuncTool{
		ToolName: confirmGatedTool,
		Desc:     "Delete a record by id (a state-mutating write).",
		Params:   map[string]any{"type": "object", "properties": map[string]any{"id": map[string]any{"type": "string"}}},
		Fn: func(_ context.Context, _ map[string]any) (string, error) {
			return "Record 42 deleted.", nil
		},
	}}

	ls, err := SpawnLocal(
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(mock),
		WithLocalServerOption(WithTools(tools)),
		WithLocalServerOption(WithConfirmTools([]string{confirmGatedTool})),
	)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	return ls
}

// connectTransport opens a raw WS transport to the server (asserting the exact wire
// frames, like the scenario runner).
func connectTransport(t *testing.T, ls *LocalServer) *protocol.WebSocketTransport {
	t.Helper()
	transport := protocol.NewWebSocketTransport(ls.WSURL(), nil)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := transport.Connect(ctx); err != nil {
		t.Fatalf("connect transport: %v", err)
	}
	return transport
}

// sendFrame marshals + sends one client frame.
func sendFrame(t *testing.T, transport protocol.Transport, frame map[string]any) {
	t.Helper()
	payload, err := json.Marshal(frame)
	if err != nil {
		t.Fatalf("marshal frame: %v", err)
	}
	if err := transport.Send(payload); err != nil {
		t.Fatalf("send frame: %v", err)
	}
}

// nextEv reads the next non-keepalive/pong server event off the transport.
func nextEv(t *testing.T, transport protocol.Transport) map[string]any {
	t.Helper()
	for {
		select {
		case data, ok := <-transport.Receive():
			if !ok {
				if err := transport.Err(); err != nil {
					t.Fatalf("transport closed with error: %v", err)
				}
				t.Fatalf("transport closed before expected event")
			}
			var ev map[string]any
			if err := json.Unmarshal(data, &ev); err != nil {
				t.Fatalf("decode event: %v (raw=%s)", err, data)
			}
			if typ, _ := ev["type"].(string); typ == "keepalive" || typ == "pong" {
				continue
			}
			return ev
		case <-time.After(5 * time.Second):
			t.Fatalf("timed out waiting for next event")
			return nil
		}
	}
}

// expectType asserts the next event has the given type and returns it.
func expectType(t *testing.T, transport protocol.Transport, typ string) map[string]any {
	t.Helper()
	ev := nextEv(t, transport)
	if got, _ := ev["type"].(string); got != typ {
		t.Fatalf("expected event type %q, got %q (event=%s)", typ, got, mustJSON(ev))
	}
	return ev
}

// createSession runs the create_conversation_session handshake and returns the sessionId.
func createSession(t *testing.T, transport protocol.Transport) string {
	t.Helper()
	sendFrame(t, transport, map[string]any{
		"action":    "create_conversation_session",
		"requestId": "r-create",
		"agentId":   "11111111-1111-1111-1111-111111111111",
		"userName":  "Alice",
		"userEmail": "alice@example.com",
	})
	ev := expectType(t, transport, "immediate_response")
	data, _ := ev["data"].(map[string]any)
	sid, _ := data["sessionId"].(string)
	if sid == "" {
		t.Fatalf("create session returned no sessionId (event=%s)", mustJSON(ev))
	}
	return sid
}

// driveToConfirmation sends the gated message and asserts the park: a 202 ack, then a
// write_confirmation_required naming the gated tool, then the deferred toolCall chunk.
func driveToConfirmation(t *testing.T, transport protocol.Transport, sessionID string) {
	t.Helper()
	sendFrame(t, transport, map[string]any{
		"action":    "send_message",
		"requestId": "r-msg",
		"sessionId": sessionID,
		"message":   "delete record 42",
	})

	ack := expectType(t, transport, "immediate_response")
	if status, _ := asInt(ack["status"]); status != 202 {
		t.Fatalf("expected 202 ack, got %v", ack["status"])
	}

	// The turn parks: write_confirmation_required carries the requestId + the tool name
	// as the opaque toolId.
	park := expectType(t, transport, "write_confirmation_required")
	if rid, _ := park["requestId"].(string); rid != "r-msg" {
		t.Fatalf("write_confirmation_required requestId = %q, want r-msg", rid)
	}
	toolID, ok := dot(t, park, "data.data.toolId")
	if !ok || toolID != confirmGatedTool {
		t.Fatalf("write_confirmation_required toolId = %v, want %q (event=%s)", toolID, confirmGatedTool, mustJSON(park))
	}
	// The gated tool's toolCall chunk is DEFERRED until after the prompt.
	chunk := expectType(t, transport, "stream_chunk")
	name, ok := dot(t, chunk, "data.state.rawResponse.toolCall.name")
	if !ok || name != confirmGatedTool {
		t.Fatalf("deferred toolCall chunk name = %v, want %q (event=%s)", name, confirmGatedTool, mustJSON(chunk))
	}
}

// TestConfirmToolActionApproved drives the full approve path over the wire: the turn
// parks, the client approves, the server acks (approved=true), the tool runs (its result
// reaches the model via a stream_chunk), the model's reply streams, and the turn
// completes with the wrap-up.
func TestConfirmToolActionApproved(t *testing.T) {
	ls := hitlServer(t)
	defer ls.Shutdown()
	transport := connectTransport(t, ls)
	defer transport.Close()

	sessionID := createSession(t, transport)
	driveToConfirmation(t, transport, sessionID)

	// Approve → ack, the gated tool runs, the model wraps up.
	sendFrame(t, transport, map[string]any{
		"action":    "confirm_tool_action",
		"requestId": "r-confirm",
		"sessionId": sessionID,
		"approved":  true,
	})

	ack := expectType(t, transport, "immediate_response")
	if status, _ := asInt(ack["status"]); status != 200 {
		t.Fatalf("confirm ack status = %v, want 200", ack["status"])
	}
	if approved, _ := dot(t, ack, "data.approved"); approved != true {
		t.Fatalf("confirm ack data.approved = %v, want true (event=%s)", approved, mustJSON(ack))
	}

	// The tool result reaches the model (the real tool output, not a denial).
	result := expectType(t, transport, "stream_chunk")
	name, _ := dot(t, result, "data.state.rawResponse.toolResult.name")
	if name != confirmGatedTool {
		t.Fatalf("toolResult chunk name = %v, want %q (event=%s)", name, confirmGatedTool, mustJSON(result))
	}
	resultText, _ := dot(t, result, "data.state.rawResponse.toolResult.result")
	if resultText != "Record 42 deleted." {
		t.Fatalf("approved tool result = %v, want the real tool output (event=%s)", resultText, mustJSON(result))
	}
	if isErr, _ := dot(t, result, "data.state.rawResponse.toolResult.isError"); isErr == true {
		t.Fatalf("approved tool result should not be an error (event=%s)", mustJSON(result))
	}

	// The model's reply streams, then the turn completes.
	reply := drainReply(t, transport)
	if reply != "Done — record 42 was deleted." {
		t.Fatalf("streamed reply = %q, want the wrap-up", reply)
	}
}

// TestConfirmToolActionRejected drives the full reject path: the turn parks, the client
// rejects, the server acks (approved=false), the gated tool is BLOCKED (the model sees a
// denial, not the real output), and the turn still completes (no hang).
func TestConfirmToolActionRejected(t *testing.T) {
	ls := hitlServer(t)
	defer ls.Shutdown()
	transport := connectTransport(t, ls)
	defer transport.Close()

	sessionID := createSession(t, transport)
	driveToConfirmation(t, transport, sessionID)

	// Reject → ack, the tool is blocked, the model still wraps up.
	sendFrame(t, transport, map[string]any{
		"action":    "confirm_tool_action",
		"requestId": "r-reject",
		"sessionId": sessionID,
		"approved":  false,
	})

	ack := expectType(t, transport, "immediate_response")
	if status, _ := asInt(ack["status"]); status != 200 {
		t.Fatalf("reject ack status = %v, want 200", ack["status"])
	}
	if approved, _ := dot(t, ack, "data.approved"); approved != false {
		t.Fatalf("reject ack data.approved = %v, want false (event=%s)", approved, mustJSON(ack))
	}

	// The blocked tool's result is a denial (the engine folds the gate's Deny into the
	// tool-result string) flagged isError, NOT the real "Record 42 deleted." output.
	result := expectType(t, transport, "stream_chunk")
	resultText, _ := dot(t, result, "data.state.rawResponse.toolResult.result")
	rs, _ := resultText.(string)
	if rs == "Record 42 deleted." {
		t.Fatalf("a rejected tool must NOT run — leaked real output (event=%s)", mustJSON(result))
	}
	if !contains(rs, "Denied by human") {
		t.Fatalf("rejected tool result should carry the denial, got %q (event=%s)", rs, mustJSON(result))
	}
	if isErr, _ := dot(t, result, "data.state.rawResponse.toolResult.isError"); isErr != true {
		t.Fatalf("rejected tool result should be flagged isError (event=%s)", mustJSON(result))
	}

	// The turn still finishes cleanly with the model's wrap-up (no hang).
	reply := drainReply(t, transport)
	if reply != "Done — record 42 was deleted." {
		t.Fatalf("streamed reply = %q, want the wrap-up", reply)
	}
}

// TestConfirmToolActionDuplicateIsNoOp asserts a confirm for a session with nothing
// parked (here: a duplicate after the turn already resumed) is a clean
// NO_PENDING_CONFIRMATION error, not a silent approve — matching the Rust reference's
// duplicate-confirm assertion.
func TestConfirmToolActionDuplicateIsNoOp(t *testing.T) {
	ls := hitlServer(t)
	defer ls.Shutdown()
	transport := connectTransport(t, ls)
	defer transport.Close()

	sessionID := createSession(t, transport)
	driveToConfirmation(t, transport, sessionID)

	sendFrame(t, transport, map[string]any{
		"action":    "confirm_tool_action",
		"requestId": "r-confirm",
		"sessionId": sessionID,
		"approved":  true,
	})
	// Consume the ack + the resumed stream to completion so the registry is cleared.
	expectType(t, transport, "immediate_response") // confirm ack
	expectType(t, transport, "stream_chunk")       // tool result
	drainReply(t, transport)

	// A second confirm for the same session now has nothing parked → fail-closed error.
	sendFrame(t, transport, map[string]any{
		"action":    "confirm_tool_action",
		"requestId": "r-dup",
		"sessionId": sessionID,
		"approved":  true,
	})
	dup := expectType(t, transport, "error")
	if code, _ := dot(t, dup, "error.code"); code != "NO_PENDING_CONFIRMATION" {
		t.Fatalf("duplicate confirm error code = %v, want NO_PENDING_CONFIRMATION (event=%s)", code, mustJSON(dup))
	}
}

// TestConfirmToolActionFailsClosed asserts a confirm_tool_action with a missing
// 'approved' verdict is rejected (never silently approve a write).
func TestConfirmToolActionFailsClosed(t *testing.T) {
	ls := hitlServer(t)
	defer ls.Shutdown()
	transport := connectTransport(t, ls)
	defer transport.Close()

	sessionID := createSession(t, transport)
	driveToConfirmation(t, transport, sessionID)

	// No 'approved' field → VALIDATION_ERROR (fail closed), and the turn stays parked.
	sendFrame(t, transport, map[string]any{
		"action":    "confirm_tool_action",
		"requestId": "r-bad",
		"sessionId": sessionID,
	})
	bad := expectType(t, transport, "error")
	if code, _ := dot(t, bad, "error.code"); code != "VALIDATION_ERROR" {
		t.Fatalf("missing-approved error code = %v, want VALIDATION_ERROR (event=%s)", code, mustJSON(bad))
	}

	// The turn is still parked: a real approve now resumes it (proving the bad frame did
	// not consume / mis-route the pending confirmation).
	sendFrame(t, transport, map[string]any{
		"action":    "confirm_tool_action",
		"requestId": "r-confirm",
		"sessionId": sessionID,
		"approved":  true,
	})
	ack := expectType(t, transport, "immediate_response")
	if approved, _ := dot(t, ack, "data.approved"); approved != true {
		t.Fatalf("recovered confirm ack data.approved = %v, want true (event=%s)", approved, mustJSON(ack))
	}
}

// drainReply consumes stream_token events (accumulating the reply text) up to and
// including the terminal eventual_response, returning the accumulated reply.
func drainReply(t *testing.T, transport protocol.Transport) string {
	t.Helper()
	reply := ""
	for {
		ev := nextEv(t, transport)
		switch ev["type"] {
		case "stream_token":
			if tok, ok := ev["token"].(string); ok {
				reply += tok
			}
		case "eventual_response":
			return reply
		case "stream_chunk":
			// tolerate interleaved chunks (none expected here, but don't fail the drain)
		default:
			t.Fatalf("unexpected event while draining reply: %s", mustJSON(ev))
		}
	}
}

// contains is a tiny substring helper (avoids importing strings just for this).
func contains(s, sub string) bool {
	return len(sub) == 0 || (len(s) >= len(sub) && indexOf(s, sub) >= 0)
}

func indexOf(s, sub string) int {
	for i := 0; i+len(sub) <= len(s); i++ {
		if s[i:i+len(sub)] == sub {
			return i
		}
	}
	return -1
}
