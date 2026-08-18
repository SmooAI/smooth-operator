package server

import (
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/SmooAI/smooth-operator/go/protocol"
)

// Rich Interactions park/resume over the real WebSocket transport — the Go port of the
// Rust rust/smooth-operator-server/tests/submit_interaction.rs. Drives the choices kind
// end to end against a live server with a MockLlmProvider scripting the request_choices
// raise (offline, no gateway):
//
//   - rich session (declared choice_chips): raise → interaction_required → submit_interaction
//     → the turn resumes with the validated values reaching the model → reply;
//   - invalid submit stays parked (interaction_invalid) and a corrected submit resumes;
//   - fallback session (no choice_chips): the raise degrades to the conversational
//     directive — NO interaction_required, the turn completes without parking.

// choicesQuestionsArg is the request_choices tool argument the mock scripts: one
// single-select question. Small on purpose so the assertions stay legible.
const choicesQuestionsArg = `{"questions":[{"question":"Which plan interests you?","header":"Plan","options":[{"label":"Basic"},{"label":"Pro"}]}],"reason":"to route you"}`

// choicesServer spins up a local server whose mock LLM scripts a request_choices call
// followed by a final text reply. The choices kind is hosted by default
// (DefaultInteractionKinds), so no extra wiring is needed beyond the mock.
func choicesServer(t *testing.T) *LocalServer {
	t.Helper()
	mock := core.NewMockLlmProvider()
	mock.PushToolCall("call-1", "request_choices", choicesQuestionsArg)
	mock.PushText("Great — I'll set you up on the Pro plan.")

	ls, err := SpawnLocal(
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(mock),
	)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	return ls
}

// createSessionSupports runs create_conversation_session declaring the given render
// capabilities and returns the sessionId (the shared createSession helper declares none).
func createSessionSupports(t *testing.T, transport protocol.Transport, supports []string) string {
	t.Helper()
	frame := map[string]any{
		"action":    "create_conversation_session",
		"requestId": "r-create",
		"agentId":   "11111111-1111-1111-1111-111111111111",
		"userName":  "Alice",
		"userEmail": "alice@example.com",
	}
	if supports != nil {
		frame["supports"] = supports
	}
	sendFrame(t, transport, frame)
	ev := expectType(t, transport, "immediate_response")
	data, _ := ev["data"].(map[string]any)
	sid, _ := data["sessionId"].(string)
	if sid == "" {
		t.Fatalf("create session returned no sessionId (event=%s)", mustJSON(ev))
	}
	return sid
}

// TestSubmitInteractionRichPathResumes drives the full rich path: the raise parks the
// turn (interaction_required with the choices spec), the client submits a valid pick, the
// server acks, the validated values reach the model as the raise tool's result, and the
// turn completes.
func TestSubmitInteractionRichPathResumes(t *testing.T) {
	ls := choicesServer(t)
	defer ls.Shutdown()
	transport := connectTransport(t, ls)
	defer transport.Close()

	sessionID := createSessionSupports(t, transport, []string{"choice_chips"})

	sendFrame(t, transport, map[string]any{
		"action":    "send_message",
		"requestId": "r-msg",
		"sessionId": sessionID,
		"message":   "I want to sign up",
	})
	if ack := expectType(t, transport, "immediate_response"); mustStatus(t, ack) != 202 {
		t.Fatalf("expected 202 ack, got %v", ack["status"])
	}

	// The raise tool's toolCall chunk is emitted (deterministically) before the park.
	call := expectType(t, transport, "stream_chunk")
	if name, _ := dot(t, call, "data.state.rawResponse.toolCall.name"); name != "request_choices" {
		t.Fatalf("expected request_choices toolCall chunk, got %v (event=%s)", name, mustJSON(call))
	}

	// The turn PARKS: interaction_required carries the kind, the spec, and an interactionId.
	req := expectType(t, transport, "interaction_required")
	if rid, _ := req["requestId"].(string); rid != "r-msg" {
		t.Fatalf("interaction_required requestId = %q, want r-msg", rid)
	}
	kind, _ := dot(t, req, "data.data.kind")
	if kind != "choices" {
		t.Fatalf("interaction_required kind = %v, want choices (event=%s)", kind, mustJSON(req))
	}
	interactionID, _ := dot(t, req, "data.data.interactionId")
	iid, _ := interactionID.(string)
	if iid == "" {
		t.Fatalf("interaction_required carried no interactionId (event=%s)", mustJSON(req))
	}
	if header, _ := dot(t, req, "data.data.spec.questions.0.header"); header != "Plan" {
		t.Fatalf("interaction_required spec question header = %v, want Plan (event=%s)", header, mustJSON(req))
	}

	// Submit a valid pick → the server acks and the parked raise resumes. The ack and the
	// resumed tool-result chunk come from different goroutines, so collect the tail and
	// assert on its contents rather than a strict interleaving.
	sendFrame(t, transport, map[string]any{
		"action":        "submit_interaction",
		"requestId":     "r-msg",
		"sessionId":     sessionID,
		"interactionId": iid,
		"kind":          "choices",
		"values":        map[string]any{"answers": []any{map[string]any{"header": "Plan", "options": []any{"Pro"}}}},
	})

	tail := collectUntil(t, transport, "eventual_response")
	if !hasAckFor(t, tail, iid) {
		t.Fatalf("expected a 200 submit ack echoing interactionId %q, tail=%s", iid, mustJSON(tail))
	}
	res := findToolResult(t, tail, "request_choices")
	if !contains(res, "submitted") || !contains(res, "Pro") {
		t.Fatalf("raise tool result should carry the submitted pick, got %q", res)
	}
	if reply := replyFrom(tail); reply != "Great — I'll set you up on the Pro plan." {
		t.Fatalf("streamed reply = %q, want the wrap-up", reply)
	}
}

// TestSubmitInteractionInvalidStaysParked drives the retryable path: an invalid submit
// (a label not offered) returns interaction_invalid WITHOUT resuming the turn, and a
// corrected submit then resumes it.
func TestSubmitInteractionInvalidStaysParked(t *testing.T) {
	ls := choicesServer(t)
	defer ls.Shutdown()
	transport := connectTransport(t, ls)
	defer transport.Close()

	sessionID := createSessionSupports(t, transport, []string{"choice_chips"})
	sendFrame(t, transport, map[string]any{
		"action": "send_message", "requestId": "r-msg", "sessionId": sessionID, "message": "sign me up",
	})
	expectType(t, transport, "immediate_response") // 202
	expectType(t, transport, "stream_chunk")       // request_choices toolCall
	req := expectType(t, transport, "interaction_required")
	iid, _ := mustDotString(t, req, "data.data.interactionId")

	// Invalid pick (Platinum isn't offered) → interaction_invalid, turn STAYS parked.
	sendFrame(t, transport, map[string]any{
		"action": "submit_interaction", "requestId": "r-msg", "sessionId": sessionID,
		"interactionId": iid, "kind": "choices",
		"values": map[string]any{"answers": []any{map[string]any{"header": "Plan", "options": []any{"Platinum"}}}},
	})
	invalid := expectType(t, transport, "interaction_invalid")
	field, _ := dot(t, invalid, "data.data.errors.0.field")
	if field != "Plan" {
		t.Fatalf("interaction_invalid error field = %v, want Plan (event=%s)", field, mustJSON(invalid))
	}

	// A corrected submit resumes the still-parked turn (same interactionId — the park
	// survived the invalid attempt).
	sendFrame(t, transport, map[string]any{
		"action": "submit_interaction", "requestId": "r-msg", "sessionId": sessionID,
		"interactionId": iid, "kind": "choices",
		"values": map[string]any{"answers": []any{map[string]any{"header": "Plan", "options": []any{"Basic"}}}},
	})
	tail := collectUntil(t, transport, "eventual_response")
	if !hasAckFor(t, tail, iid) {
		t.Fatalf("expected a 200 submit ack after the corrected submit, tail=%s", mustJSON(tail))
	}
	if res := findToolResult(t, tail, "request_choices"); !contains(res, "Basic") {
		t.Fatalf("resumed tool result should carry the corrected pick Basic, got %q", res)
	}
}

// TestChoicesFallbackNoCapabilityDoesNotPark drives the text-only fallback: a session
// that did NOT declare choice_chips gets the conversational directive from the raise tool
// (no interaction_required, no park) and the turn completes normally.
func TestChoicesFallbackNoCapabilityDoesNotPark(t *testing.T) {
	ls := choicesServer(t)
	defer ls.Shutdown()
	transport := connectTransport(t, ls)
	defer transport.Close()

	// No `supports` → text-only channel.
	sessionID := createSessionSupports(t, transport, nil)
	sendFrame(t, transport, map[string]any{
		"action": "send_message", "requestId": "r-msg", "sessionId": sessionID, "message": "sign me up",
	})
	expectType(t, transport, "immediate_response") // 202

	// The raise tool degrades to the fallback directive — the model sees it as a tool
	// result, and there is NO interaction_required / park anywhere in the turn.
	tail := collectUntil(t, transport, "eventual_response")
	for _, ev := range tail {
		if typ, _ := ev["type"].(string); typ == "interaction_required" {
			t.Fatalf("a fallback session must NOT park (got interaction_required): %s", mustJSON(ev))
		}
	}
	res := findToolResult(t, tail, "request_choices")
	if !contains(res, "conversational") || !contains(res, "Basic, Pro") {
		t.Fatalf("fallback tool result should carry the conversational directive, got %q", res)
	}
	// The turn completes normally (no parked interaction to resume).
	if reply := replyFrom(tail); reply == "" {
		t.Fatalf("expected a wrap-up reply on the fallback path")
	}
}

// mustStatus pulls the integer status off an immediate_response (fatal if absent).
func mustStatus(t *testing.T, ev map[string]any) int {
	t.Helper()
	s, ok := asInt(ev["status"])
	if !ok {
		t.Fatalf("event has no integer status: %s", mustJSON(ev))
	}
	return s
}

// mustDotString reads a dotted path as a non-empty string (fatal otherwise).
func mustDotString(t *testing.T, obj map[string]any, path string) (string, bool) {
	t.Helper()
	v, ok := dot(t, obj, path)
	s, _ := v.(string)
	if !ok || s == "" {
		t.Fatalf("path %q missing or empty (obj=%s)", path, mustJSON(obj))
	}
	return s, true
}

// collectUntil reads events until (and including) the first of the given type, returning
// all collected events. Used where two goroutines emit into the sink and the exact
// interleaving isn't guaranteed (the submit ack vs the resumed tool-result chunk).
func collectUntil(t *testing.T, transport protocol.Transport, typ string) []map[string]any {
	t.Helper()
	var out []map[string]any
	for {
		ev := nextEv(t, transport)
		out = append(out, ev)
		if got, _ := ev["type"].(string); got == typ {
			return out
		}
	}
}

// hasAckFor reports whether the collected events include a 200 immediate_response whose
// data.interactionId echoes iid (the submit ack).
func hasAckFor(t *testing.T, events []map[string]any, iid string) bool {
	t.Helper()
	for _, ev := range events {
		if typ, _ := ev["type"].(string); typ != "immediate_response" {
			continue
		}
		if s, ok := asInt(ev["status"]); !ok || s != 200 {
			continue
		}
		if got, _ := dot(t, ev, "data.interactionId"); got == iid {
			return true
		}
	}
	return false
}

// findToolResult returns the result string of the first stream_chunk toolResult for the
// named tool (fatal if none present).
func findToolResult(t *testing.T, events []map[string]any, tool string) string {
	t.Helper()
	for _, ev := range events {
		if typ, _ := ev["type"].(string); typ != "stream_chunk" {
			continue
		}
		name, ok := dot(t, ev, "data.state.rawResponse.toolResult.name")
		if !ok || name != tool {
			continue
		}
		res, _ := dot(t, ev, "data.state.rawResponse.toolResult.result")
		s, _ := res.(string)
		return s
	}
	t.Fatalf("no toolResult chunk for %q in %s", tool, mustJSON(events))
	return ""
}

// replyFrom accumulates the reply text from the collected stream_token events.
func replyFrom(events []map[string]any) string {
	reply := ""
	for _, ev := range events {
		if typ, _ := ev["type"].(string); typ == "stream_token" {
			if tok, ok := ev["token"].(string); ok {
				reply += tok
			}
		}
	}
	return reply
}
