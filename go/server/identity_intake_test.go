package server

import (
	"encoding/json"
	"testing"
)

// Unit tests for the identity_intake Rich Interaction kind — the Go port of the Rust
// rust/smooth-operator/src/identity_intake.rs tests, plus validation against the shared
// conformance fixtures (spec/conformance/fixtures.json), so the Go validator agrees with
// the Rust reference on the same wire shapes.

func ifield(key string, required bool) IntakeField {
	return IntakeField{Key: key, Required: required}
}

func TestNormalizeEmailShapes(t *testing.T) {
	if got, ok := normalizeEmail("Alice@Example.COM"); !ok || got != "Alice@example.com" {
		t.Fatalf("domain lowercased/local preserved: got %q ok=%v", got, ok)
	}
	for _, bad := range []string{"", "no-at", "@x.com", "a@b", "a@.com", "a@b.", "a b@c.com", "a@b@c.com"} {
		if _, ok := normalizeEmail(bad); ok {
			t.Errorf("%q should be rejected", bad)
		}
	}
}

func TestNormalizePhoneE164(t *testing.T) {
	cases := map[string]string{
		"+1 (555) 123-4567": "+15551234567",
		"555.123.4567":      "+15551234567",  // bare 10-digit NANP
		"1 555 123 4567":    "+15551234567",  // 1-prefixed 11-digit NANP
		"+447911123456":     "+447911123456", // non-NANP with country code
	}
	for in, want := range cases {
		if got, ok := normalizePhoneE164(in); !ok || got != want {
			t.Errorf("normalizePhoneE164(%q) = %q,%v want %q", in, got, ok, want)
		}
	}
	for _, bad := range []string{"", "abc", "+0123456789", "12345", "+1234567890123456"} {
		if _, ok := normalizePhoneE164(bad); ok {
			t.Errorf("%q should be rejected", bad)
		}
	}
}

func TestValidateIntakeRequiredMissingIsError(t *testing.T) {
	fields := []IntakeField{ifield("email", true), ifield("name", false)}
	_, errs := validateIntake(fields, IntakeValues{})
	if len(errs) != 1 || errs[0].Field != "email" {
		t.Fatalf("expected one email-required error, got %+v", errs)
	}
	// Blank counts as missing.
	if _, errs := validateIntake(fields, IntakeValues{Email: "   "}); len(errs) == 0 {
		t.Fatal("blank required email should error")
	}
}

func TestValidateIntakeValidNormalizes(t *testing.T) {
	fields := []IntakeField{ifield("email", true), ifield("phone", false)}
	out, errs := validateIntake(fields, IntakeValues{
		Name:  "  Alice Example  ",
		Email: "alice@Example.com",
		Phone: "(555) 123-4567",
	})
	if errs != nil {
		t.Fatalf("expected valid, got %+v", errs)
	}
	if out.Name != "Alice Example" || out.Email != "alice@example.com" || out.Phone != "+15551234567" {
		t.Fatalf("unexpected normalization: %+v", out)
	}
}

func TestValidateIntakeAllErrorsInOnePass(t *testing.T) {
	fields := []IntakeField{ifield("name", true)}
	_, errs := validateIntake(fields, IntakeValues{Email: "not-an-email", Phone: "nope"})
	if len(errs) != 3 {
		t.Fatalf("missing name + bad email + bad phone should be 3 errors, got %+v", errs)
	}
}

func TestValidateIntakeVolunteeredFieldsKept(t *testing.T) {
	// Only email requested, but the visitor volunteered a phone — keep it.
	fields := []IntakeField{ifield("email", true)}
	out, errs := validateIntake(fields, IntakeValues{Email: "a@b.co", Phone: "+15551234567"})
	if errs != nil {
		t.Fatalf("expected valid, got %+v", errs)
	}
	if out.Phone != "+15551234567" {
		t.Fatalf("volunteered phone dropped: %+v", out)
	}
}

func TestIdentityIntakeParseRequestShorthandAndUnknown(t *testing.T) {
	req, err := IdentityIntakeKind{}.ParseRequest(map[string]any{
		"fields": []any{"email", map[string]any{"key": "name", "required": false}},
		"reason": "to send you the quote",
	})
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	var spec identityIntakeSpec
	if err := json.Unmarshal(req.Spec, &spec); err != nil {
		t.Fatalf("spec unmarshal: %v", err)
	}
	// Shorthand string "email" ⇒ required; explicit object keeps required:false.
	if len(spec.Fields) != 2 || !spec.Fields[0].Required || spec.Fields[1].Required {
		t.Fatalf("unexpected fields: %+v", spec.Fields)
	}
	if _, err := (IdentityIntakeKind{}).ParseRequest(map[string]any{"fields": []any{"ssn"}, "reason": "x"}); err == nil {
		t.Fatal("unknown field key should error")
	}
}

// TestIdentityIntakeFixturesValidate cross-checks the Go kind against the shared conformance
// fixtures the Rust reference validates against: identity_intake_values against
// identity_intake_spec must validate to the identity_intake_payload's normalized values.
func TestIdentityIntakeFixturesValidate(t *testing.T) {
	fixtures := loadFixtures(t)
	spec := fixtureInstance(t, fixtures, "identity_intake_spec")
	values := fixtureInstance(t, fixtures, "identity_intake_values")
	payload := fixtureInstance(t, fixtures, "identity_intake_payload")

	specJSON, err := json.Marshal(spec)
	if err != nil {
		t.Fatalf("marshal spec: %v", err)
	}
	canonical, errs := IdentityIntakeKind{}.Validate(specJSON, values)
	if errs != nil {
		t.Fatalf("shared identity_intake_values should validate against the spec, got %v", errs)
	}
	if !jsonEqual(canonical, payload["values"]) {
		got, _ := json.Marshal(canonical)
		want, _ := json.Marshal(payload["values"])
		t.Fatalf("canonical values != fixture payload values\n got: %s\nwant: %s", got, want)
	}
	if payload["status"] != "submitted" {
		t.Fatalf("fixture payload status = %v, want submitted", payload["status"])
	}
}
