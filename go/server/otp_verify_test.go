package server

import (
	"context"
	"encoding/json"
	"errors"
	"testing"
)

// fakeOtp is a scripted OtpService for tests. It records what it was asked and returns the
// preset delivery/outcome, so a test can assert the server orchestrates the seam correctly
// without any real code generation/delivery.
type fakeOtp struct {
	delivery    OtpDelivery
	sendErr     error
	outcome     OtpVerifyOutcome
	sentSession string
	sentContact OtpContact
	verifiedARG string
}

func (f *fakeOtp) SendOtp(_ context.Context, sessionID string, contact OtpContact) (OtpDelivery, error) {
	f.sentSession = sessionID
	f.sentContact = contact
	if f.sendErr != nil {
		return OtpDelivery{}, f.sendErr
	}
	return f.delivery, nil
}

func (f *fakeOtp) VerifyOtp(_ context.Context, _ /*sessionID*/, code string) OtpVerifyOutcome {
	f.verifiedARG = code
	return f.outcome
}

// otpDispatcher builds a bare dispatcher wired only with a store + OTP service — enough to
// drive verify_otp / offer-flow handlers directly (no client/tools needed).
func otpDispatcher(store SessionStore, svc OtpService) *FrameDispatcher {
	return NewFrameDispatcher(store, nil, AccessContext{}, "", nil, nil, nil, nil, nil, "", nil, nil, svc)
}

// capture returns a sink that appends every emitted event, and the slice it fills.
func capture() (EventSink, *[]map[string]any) {
	var events []map[string]any
	sink := func(e map[string]any) { events = append(events, e) }
	return sink, &events
}

// dispatchJSON routes one raw frame through the dispatcher.
func dispatchJSON(t *testing.T, d *FrameDispatcher, frame map[string]any, sink EventSink) {
	t.Helper()
	raw, err := json.Marshal(frame)
	if err != nil {
		t.Fatalf("marshal frame: %v", err)
	}
	d.Dispatch(context.Background(), raw, sink)
}

// newSession creates a session in the store and returns its id (contact email captured).
func newSession(t *testing.T, store SessionStore, email string) string {
	t.Helper()
	s, err := store.CreateSession(context.Background(), "agent-1", "Alice", email)
	if err != nil {
		t.Fatalf("create session: %v", err)
	}
	return s.SessionID
}

func TestHandleVerifyOtp(t *testing.T) {
	t.Run("happy path emits otp_verified and marks session authenticated", func(t *testing.T) {
		store := NewInMemorySessionStore()
		sid := newSession(t, store, "alice@example.com")
		svc := &fakeOtp{outcome: Verified()}
		d := otpDispatcher(store, svc)
		sink, events := capture()

		dispatchJSON(t, d, map[string]any{"action": "verify_otp", "requestId": "r1", "sessionId": sid, "code": "123456"}, sink)

		if len(*events) != 1 || (*events)[0]["type"] != "otp_verified" {
			t.Fatalf("want a single otp_verified, got %+v", *events)
		}
		if svc.verifiedARG != "123456" {
			t.Errorf("service saw code %q, want 123456", svc.verifiedARG)
		}
		s, _ := store.GetSession(context.Background(), sid)
		if !s.OtpVerified {
			t.Error("session must be marked OtpVerified after success")
		}
	})

	t.Run("invalid code emits otp_invalid with host attempts, no auth", func(t *testing.T) {
		store := NewInMemorySessionStore()
		sid := newSession(t, store, "alice@example.com")
		svc := &fakeOtp{outcome: Invalid(2, OtpErrorInvalidCode, "Invalid code. 2 attempt(s) remaining.")}
		d := otpDispatcher(store, svc)
		sink, events := capture()

		dispatchJSON(t, d, map[string]any{"action": "verify_otp", "requestId": "r1", "sessionId": sid, "code": "000000"}, sink)

		ev := singleEvent(t, events, "otp_invalid")
		inner := dataData(t, ev)
		if inner["error"] != "INVALID_CODE" || mustInt(t, inner["attemptsRemaining"]) != 2 {
			t.Errorf("otp_invalid payload = %+v", inner)
		}
		s, _ := store.GetSession(context.Background(), sid)
		if s.OtpVerified {
			t.Error("a rejected code must NOT authenticate the session")
		}
	})

	t.Run("no service fails closed with otp_invalid NOT_FOUND", func(t *testing.T) {
		store := NewInMemorySessionStore()
		sid := newSession(t, store, "alice@example.com")
		d := otpDispatcher(store, nil) // no OtpService installed
		sink, events := capture()

		dispatchJSON(t, d, map[string]any{"action": "verify_otp", "requestId": "r1", "sessionId": sid, "code": "123456"}, sink)

		ev := singleEvent(t, events, "otp_invalid")
		inner := dataData(t, ev)
		if inner["error"] != "NOT_FOUND" || mustInt(t, inner["attemptsRemaining"]) != 0 {
			t.Errorf("fail-closed payload = %+v", inner)
		}
	})

	t.Run("unknown session errors SESSION_NOT_FOUND", func(t *testing.T) {
		store := NewInMemorySessionStore()
		svc := &fakeOtp{outcome: Verified()}
		d := otpDispatcher(store, svc)
		sink, events := capture()

		dispatchJSON(t, d, map[string]any{"action": "verify_otp", "requestId": "r1", "sessionId": "nope", "code": "123456"}, sink)

		ev := singleEvent(t, events, "error")
		if code := errorCode(t, ev); code != "SESSION_NOT_FOUND" {
			t.Errorf("error code = %q, want SESSION_NOT_FOUND", code)
		}
		if svc.verifiedARG != "" {
			t.Error("VerifyOtp must not be called for an unknown session")
		}
	})

	t.Run("missing fields error in order requestId → sessionId → code", func(t *testing.T) {
		store := NewInMemorySessionStore()
		sid := newSession(t, store, "alice@example.com")
		d := otpDispatcher(store, &fakeOtp{outcome: Verified()})

		// No requestId.
		sink, events := capture()
		dispatchJSON(t, d, map[string]any{"action": "verify_otp", "sessionId": sid, "code": "1"}, sink)
		if ev := singleEvent(t, events, "error"); errorCode(t, ev) != "VALIDATION_ERROR" {
			t.Errorf("missing requestId → %+v", ev)
		}

		// No sessionId.
		sink, events = capture()
		dispatchJSON(t, d, map[string]any{"action": "verify_otp", "requestId": "r1", "code": "1"}, sink)
		if ev := singleEvent(t, events, "error"); errorCode(t, ev) != "VALIDATION_ERROR" {
			t.Errorf("missing sessionId → %+v", ev)
		}

		// No code.
		sink, events = capture()
		dispatchJSON(t, d, map[string]any{"action": "verify_otp", "requestId": "r1", "sessionId": sid}, sink)
		if ev := singleEvent(t, events, "error"); errorCode(t, ev) != "VALIDATION_ERROR" {
			t.Errorf("missing code → %+v", ev)
		}
	})
}

// singleEvent asserts exactly one event of the given type was emitted and returns it.
func singleEvent(t *testing.T, events *[]map[string]any, typ string) map[string]any {
	t.Helper()
	if len(*events) != 1 {
		t.Fatalf("want exactly one event, got %d: %+v", len(*events), *events)
	}
	ev := (*events)[0]
	if ev["type"] != typ {
		t.Fatalf("event type = %v, want %s", ev["type"], typ)
	}
	return ev
}

// dataData digs the double-nested data.data payload out of an event.
func dataData(t *testing.T, ev map[string]any) map[string]any {
	t.Helper()
	data, ok := ev["data"].(map[string]any)
	if !ok {
		t.Fatalf("event has no data object: %+v", ev)
	}
	inner, ok := data["data"].(map[string]any)
	if !ok {
		t.Fatalf("event has no data.data object: %+v", ev)
	}
	return inner
}

// mustInt coerces a numeric field (native int from a direct-dispatch builder, or float64
// after a JSON round-trip) to int, failing the test if it isn't numeric.
func mustInt(t *testing.T, v any) int {
	t.Helper()
	n, ok := asInt(v)
	if !ok {
		t.Fatalf("value %v (%T) is not numeric", v, v)
	}
	return n
}

// errorCode pulls the top-level error.code out of an error event.
func errorCode(t *testing.T, ev map[string]any) string {
	t.Helper()
	e, ok := ev["error"].(map[string]any)
	if !ok {
		t.Fatalf("not an error event: %+v", ev)
	}
	code, _ := e["code"].(string)
	return code
}

func TestOfferOtp(t *testing.T) {
	t.Run("emits otp_verification_required then otp_sent", func(t *testing.T) {
		svc := &fakeOtp{delivery: OtpDelivery{Channel: OtpChannelEmail, MaskedDestination: "j***@example.com"}}
		d := otpDispatcher(NewInMemorySessionStore(), svc)
		sink, events := capture()

		d.offerOtp(context.Background(), "sess-1", "pay_invoice", OtpContact{Email: "j@example.com"}, "r1", sink)

		if len(*events) != 2 || (*events)[0]["type"] != "otp_verification_required" || (*events)[1]["type"] != "otp_sent" {
			t.Fatalf("want [otp_verification_required, otp_sent], got %+v", *events)
		}
		if svc.sentSession != "sess-1" || svc.sentContact.Email != "j@example.com" {
			t.Errorf("SendOtp got session=%q contact=%+v", svc.sentSession, svc.sentContact)
		}
	})

	t.Run("send failure emits otp_verification_required then error, no otp_sent", func(t *testing.T) {
		svc := &fakeOtp{sendErr: errors.New("smtp down")}
		d := otpDispatcher(NewInMemorySessionStore(), svc)
		sink, events := capture()

		d.offerOtp(context.Background(), "sess-1", "pay_invoice", OtpContact{Email: "j@example.com"}, "r1", sink)

		if len(*events) != 2 || (*events)[0]["type"] != "otp_verification_required" || (*events)[1]["type"] != "error" {
			t.Fatalf("want [otp_verification_required, error], got %+v", *events)
		}
		if code := errorCode(t, (*events)[1]); code != "OTP_SEND_FAILED" {
			t.Errorf("error code = %q, want OTP_SEND_FAILED", code)
		}
	})
}
