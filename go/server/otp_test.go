package server

import (
	"reflect"
	"sync"
	"testing"
)

func TestOtpChannelWireStrings(t *testing.T) {
	if string(OtpChannelEmail) != "email" {
		t.Errorf("email channel = %q", OtpChannelEmail)
	}
	if string(OtpChannelSMS) != "sms" {
		t.Errorf("sms channel = %q", OtpChannelSMS)
	}
}

func TestOtpErrorWireStrings(t *testing.T) {
	cases := map[OtpErrorCode]string{
		OtpErrorInvalidCode: "INVALID_CODE",
		OtpErrorMaxAttempts: "MAX_ATTEMPTS",
		OtpErrorNotFound:    "NOT_FOUND",
		OtpErrorExpired:     "EXPIRED",
	}
	for code, want := range cases {
		if string(code) != want {
			t.Errorf("%v = %q, want %q", code, string(code), want)
		}
	}
}

func TestOtpContactAvailableChannels(t *testing.T) {
	tests := []struct {
		name    string
		contact OtpContact
		empty   bool
		want    []OtpChannel
	}{
		{name: "empty offers nothing", contact: OtpContact{}, empty: true, want: nil},
		{name: "email only offers email", contact: OtpContact{Email: "a@example.com"}, empty: false, want: []OtpChannel{OtpChannelEmail}},
		{name: "phone only offers sms", contact: OtpContact{Phone: "+15551234567"}, empty: false, want: []OtpChannel{OtpChannelSMS}},
		{name: "both offer email then sms", contact: OtpContact{Email: "a@example.com", Phone: "+15551234567"}, empty: false, want: []OtpChannel{OtpChannelEmail, OtpChannelSMS}},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if tt.contact.IsEmpty() != tt.empty {
				t.Errorf("IsEmpty() = %v, want %v", tt.contact.IsEmpty(), tt.empty)
			}
			if got := tt.contact.AvailableChannels(); !reflect.DeepEqual(got, tt.want) {
				t.Errorf("AvailableChannels() = %v, want %v", got, tt.want)
			}
		})
	}
}

func TestOtpVerifyOutcomeConstructors(t *testing.T) {
	if got := Verified(); !got.OK {
		t.Errorf("Verified() should be OK: %+v", got)
	}
	inv := Invalid(2, OtpErrorInvalidCode, "try again")
	if inv.OK {
		t.Error("Invalid() must not be OK")
	}
	if inv.AttemptsRemaining != 2 || inv.Error != OtpErrorInvalidCode || inv.Message != "try again" {
		t.Errorf("Invalid() fields wrong: %+v", inv)
	}
	// An error-less rejection is representable (host couldn't determine a cause).
	if got := Invalid(0, "", "locked"); got.Error != "" {
		t.Errorf("empty error should stay empty: %+v", got)
	}
}

func TestOtpRefusalRecorder(t *testing.T) {
	// nil recorder is a safe no-op both ways.
	var nilRec *otpRefusal
	nilRec.record("x")
	if nilRec.refusedTool() != "" {
		t.Error("nil recorder must report no refusal")
	}

	r := &otpRefusal{}
	if r.refusedTool() != "" {
		t.Error("fresh recorder must report no refusal")
	}
	r.record("pay_invoice")
	if r.refusedTool() != "pay_invoice" {
		t.Errorf("refusedTool() = %q", r.refusedTool())
	}
	// Last write wins (mirrors the Rust hook overwriting the slot).
	r.record("refund")
	if r.refusedTool() != "refund" {
		t.Errorf("refusedTool() = %q, want last recorded", r.refusedTool())
	}

	// Concurrent record/read is race-clean (mutex-guarded).
	var wg sync.WaitGroup
	for i := 0; i < 20; i++ {
		wg.Add(1)
		go func() { defer wg.Done(); r.record("t"); _ = r.refusedTool() }()
	}
	wg.Wait()
}
