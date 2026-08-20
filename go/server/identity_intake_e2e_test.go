package server

import (
	"context"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// identity_intake park/resume over the real WebSocket transport — the second Rich
// Interaction kind end to end, plus the HOST-EFFECT seam. After a valid submit (rich OR
// conversational) the session metadata must carry the captured userName / contactEmail /
// contactPhone (the same keys the OTP contact seam reads), so the captured contact becomes
// OTP-contactable on the next turn.

// identityFieldsArg is the request_identity_intake argument the mock scripts: name (optional),
// email (required), phone (optional).
const identityFieldsArg = `{"fields":[{"key":"name","required":false},{"key":"email","required":true},{"key":"phone","required":false}],"reason":"to send you the quote"}`

// identityValues is the visitor's submitted values (unnormalized phone/email on purpose, to
// prove the server normalizes before stamping).
var identityValues = map[string]any{
	"name":  "  Alice Example  ",
	"email": "Alice@Example.com",
	"phone": "(555) 123-4567",
}

// identityServer spins up a local server with the given store so a test can assert the host
// effect on the session after submit. The mock scripts request_identity_intake + a wrap-up.
func identityServer(t *testing.T, store SessionStore, extraToolCalls func(m *core.MockLlmProvider)) *LocalServer {
	t.Helper()
	mock := core.NewMockLlmProvider()
	mock.PushToolCall("call-1", "request_identity_intake", identityFieldsArg)
	if extraToolCalls != nil {
		extraToolCalls(mock)
	}
	mock.PushText("Thanks Alice — I'll send the quote over.")

	ls, err := SpawnLocal(
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(mock),
		WithLocalServerOption(WithSessionStore(store)),
	)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	return ls
}

// assertIdentityStamped reads the session back and asserts the host effect stamped the
// normalized identity, and that the captured contact is now OTP-contactable (email + SMS).
func assertIdentityStamped(t *testing.T, store SessionStore, sessionID string) {
	t.Helper()
	s, err := store.GetSession(context.Background(), sessionID)
	if err != nil || s == nil {
		t.Fatalf("get session: %v (nil=%v)", err, s == nil)
	}
	if s.UserName != "Alice Example" {
		t.Errorf("UserName = %q, want normalized 'Alice Example'", s.UserName)
	}
	if s.ContactEmail != "Alice@example.com" {
		t.Errorf("ContactEmail = %q, want normalized 'Alice@example.com'", s.ContactEmail)
	}
	if s.ContactPhone != "+15551234567" {
		t.Errorf("ContactPhone = %q, want E.164 '+15551234567'", s.ContactPhone)
	}
	// OTP-contactable: the same seam the OTP flow reads (dispatcher builds OtpContact from
	// these fields). Both channels are now available.
	contact := OtpContact{Email: s.ContactEmail, Phone: s.ContactPhone}
	chans := contact.AvailableChannels()
	if len(chans) != 2 || chans[0] != OtpChannelEmail || chans[1] != OtpChannelSMS {
		t.Errorf("captured contact not OTP-contactable on both channels: %v", chans)
	}
}

// TestIdentityIntakeRichPathStampsSessionIdentity drives the rich path: raise → park
// (interaction_required) → submit_interaction → resume, and asserts the HOST EFFECT stamped
// the normalized identity onto the session.
func TestIdentityIntakeRichPathStampsSessionIdentity(t *testing.T) {
	store := NewInMemorySessionStore()
	ls := identityServer(t, store, nil)
	defer ls.Shutdown()
	transport := connectTransport(t, ls)
	defer transport.Close()

	sessionID := createSessionSupports(t, transport, []string{"identity_form"})

	sendFrame(t, transport, map[string]any{
		"action": "send_message", "requestId": "r-msg", "sessionId": sessionID, "message": "I'd like a quote",
	})
	if ack := expectType(t, transport, "immediate_response"); mustStatus(t, ack) != 202 {
		t.Fatalf("expected 202 ack, got %v", ack["status"])
	}
	// The park event precedes the raise tool's toolCall chunk (the reference order).
	req := expectType(t, transport, "interaction_required")
	if kind, _ := dot(t, req, "data.data.kind"); kind != "identity_intake" {
		t.Fatalf("interaction_required kind = %v, want identity_intake (event=%s)", kind, mustJSON(req))
	}

	call := expectType(t, transport, "stream_chunk")
	if name, _ := dot(t, call, "data.state.rawResponse.toolCall.name"); name != "request_identity_intake" {
		t.Fatalf("expected request_identity_intake toolCall, got %v", name)
	}
	iid, _ := mustDotString(t, req, "data.data.interactionId")
	if key, _ := dot(t, req, "data.data.spec.fields.1.key"); key != "email" {
		t.Fatalf("spec fields[1].key = %v, want email (event=%s)", key, mustJSON(req))
	}

	sendFrame(t, transport, map[string]any{
		"action": "submit_interaction", "requestId": "r-msg", "sessionId": sessionID,
		"interactionId": iid, "kind": "identity_intake", "values": identityValues,
	})

	tail := collectUntil(t, transport, "eventual_response")
	if !hasAckFor(t, tail, iid) {
		t.Fatalf("expected a 200 submit ack echoing interactionId %q, tail=%s", iid, mustJSON(tail))
	}
	if res := findToolResult(t, tail, "request_identity_intake"); !contains(res, "submitted") || !contains(res, "Alice@example.com") {
		t.Fatalf("raise tool result should carry the normalized submit, got %q", res)
	}
	if reply := replyFrom(tail); reply == "" {
		t.Fatalf("expected a wrap-up reply")
	}

	// HOST EFFECT: the session now carries the captured, normalized identity.
	assertIdentityStamped(t, store, sessionID)
}

// TestIdentityIntakeConversationalPathStampsSessionIdentity drives the text-only fallback:
// the raise degrades to the conversational directive (NO park), the model calls the generic
// submit_interaction TOOL with the collected values, and the same host effect fires.
func TestIdentityIntakeConversationalPathStampsSessionIdentity(t *testing.T) {
	store := NewInMemorySessionStore()
	// After the fallback directive, the model calls the submit_interaction tool.
	submitArgs := `{"kind":"identity_intake","values":{"name":"  Alice Example  ","email":"Alice@Example.com","phone":"(555) 123-4567"}}`
	ls := identityServer(t, store, func(m *core.MockLlmProvider) {
		m.PushToolCall("call-2", "submit_interaction", submitArgs)
	})
	defer ls.Shutdown()
	transport := connectTransport(t, ls)
	defer transport.Close()

	// No `supports` → text-only channel: identity_form NOT declared → fallback.
	sessionID := createSessionSupports(t, transport, nil)
	sendFrame(t, transport, map[string]any{
		"action": "send_message", "requestId": "r-msg", "sessionId": sessionID, "message": "I'd like a quote",
	})
	expectType(t, transport, "immediate_response") // 202

	tail := collectUntil(t, transport, "eventual_response")
	for _, ev := range tail {
		if typ, _ := ev["type"].(string); typ == "interaction_required" {
			t.Fatalf("a fallback session must NOT park (got interaction_required): %s", mustJSON(ev))
		}
	}
	// The raise degraded to the conversational directive...
	if res := findToolResult(t, tail, "request_identity_intake"); !contains(res, "conversational") {
		t.Fatalf("fallback raise should return the conversational directive, got %q", res)
	}
	// ...and the submit_interaction tool validated + returned submitted.
	if res := findToolResult(t, tail, "submit_interaction"); !contains(res, "submitted") || !contains(res, "Alice@example.com") {
		t.Fatalf("submit_interaction tool result should carry the normalized submit, got %q", res)
	}

	// HOST EFFECT fired on the conversational path too.
	assertIdentityStamped(t, store, sessionID)
}
