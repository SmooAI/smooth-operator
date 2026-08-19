package server

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
)

// Identity intake — channel-normalized lead/identity capture: the second Rich
// Interaction kind (the Go port of rust/smooth-operator/src/identity_intake.rs).
// The agent's request_identity_intake tool asks the visitor for name/email/phone.
//
//   - On a channel that declared the `identity_form` capability, the raise PARKS the
//     turn and the server emits interaction_required; the client's form resumes with a
//     submit_interaction action.
//   - On a text-only channel the same raise degrades to a conversational directive and
//     the model submits collected values through the generic submit_interaction tool.
//
// Both paths validate through validateIntake — one implementation, one behavior — and,
// on a successful submit, run the kind's HOST EFFECT (AttachEffect): stamp the captured
// identity onto the session metadata (userName / contactEmail / contactPhone — the same
// keys the pre-chat create path stashes and the OTP contact seam reads). Validated
// against the shared identity-intake fixtures (spec/conformance/fixtures.json).

// identityFormCapability gates the rich (parked form) path for the identity_intake kind.
const identityFormCapability = "identity_form"

// intakeFieldKeys is the closed set of identity fields intake can collect.
var intakeFieldKeys = map[string]bool{"name": true, "email": true, "phone": true}

// IntakeField is one requested identity field, as raised by request_identity_intake and
// carried on interaction_required.
type IntakeField struct {
	// Key is the field to collect: "name" | "email" | "phone".
	Key string `json:"key"`
	// Required reports whether the visitor must provide this field to submit.
	Required bool `json:"required"`
	// Label optionally overrides the client's default display label.
	Label string `json:"label,omitempty"`
}

// identityIntakeSpec is the render spec carried on interaction_required for kind identity_intake.
type identityIntakeSpec struct {
	Fields []IntakeField `json:"fields"`
}

// IntakeValues is the validated, normalized identity payload — the structured values the
// parked turn resumes with (identical on the form and conversational paths). Also the
// shape the host effect stamps onto the session.
type IntakeValues struct {
	Name  string `json:"name,omitempty"`
	Email string `json:"email,omitempty"`
	// Phone is E.164-normalized (`+15551234567`).
	Phone string `json:"phone,omitempty"`
}

// isEmpty reports that no field carries a value.
func (v IntakeValues) isEmpty() bool { return v.Name == "" && v.Email == "" && v.Phone == "" }

// validateIntake validates raw submitted values against the requested fields, returning
// the normalized IntakeValues or the full list of per-field errors.
//
// Rules (mirror identity_intake.rs):
//   - every `required` field must be present and non-blank;
//   - name: non-empty after trim;
//   - email: local@domain.tld shape (single @, dot in the domain, no whitespace); domain lowercased;
//   - phone: E.164 after stripping separators (+ + 8–15 digits); bare 10-digit or 1-prefixed
//     11-digit NANP numbers are accepted and normalized to +1…
//
// Fields not requested but present are still validated and kept (a volunteered phone is a
// gift, not an error). Returns every failed field (not just the first) so a form can
// annotate all of them in one round-trip.
func validateIntake(fields []IntakeField, values IntakeValues) (IntakeValues, []InteractionFieldError) {
	var errs []InteractionFieldError
	var out IntakeValues

	get := func(key string) string {
		switch key {
		case "name":
			return values.Name
		case "email":
			return values.Email
		case "phone":
			return values.Phone
		}
		return ""
	}

	// Required-ness: every required requested field must be present + non-blank.
	for _, f := range fields {
		if f.Required && strings.TrimSpace(get(f.Key)) == "" {
			errs = append(errs, InteractionFieldError{Field: f.Key, Message: "this field is required"})
		}
	}

	// Format validation + normalization for whatever was provided.
	if name := strings.TrimSpace(values.Name); name != "" {
		out.Name = name
	}
	if email := strings.TrimSpace(values.Email); email != "" {
		if normalized, ok := normalizeEmail(email); ok {
			out.Email = normalized
		} else {
			errs = append(errs, InteractionFieldError{Field: "email", Message: "must be a valid email address"})
		}
	}
	if phone := strings.TrimSpace(values.Phone); phone != "" {
		if normalized, ok := normalizePhoneE164(phone); ok {
			out.Phone = normalized
		} else {
			errs = append(errs, InteractionFieldError{Field: "phone", Message: "must be a valid phone number (include your country code, e.g. +1 555 123 4567)"})
		}
	}

	if len(errs) > 0 {
		return IntakeValues{}, errs
	}
	return out, nil
}

// normalizeEmail does minimal email-shape validation: exactly one @, non-empty local part,
// a dot-containing domain, no whitespace. Returns the trimmed address with a lowercased
// domain, or ok=false when malformed. A deliverability check belongs to the host's email
// service, not this protocol boundary.
func normalizeEmail(raw string) (string, bool) {
	s := strings.TrimSpace(raw)
	if strings.ContainsAny(s, " \t\r\n") {
		return "", false
	}
	local, domain, found := strings.Cut(s, "@")
	if !found || local == "" || strings.Contains(domain, "@") {
		return "", false
	}
	domainLC := strings.ToLower(domain)
	parts := strings.Split(domainLC, ".")
	if len(parts) < 2 {
		return "", false
	}
	for _, p := range parts {
		if p == "" {
			return "", false
		}
	}
	return local + "@" + domainLC, true
}

// normalizePhoneE164 normalizes a phone number to E.164, or ok=false when unparseable.
// Strips common separators, then accepts +CC (8–15 digits, no leading 0), or a bare
// 10-digit / 1-prefixed 11-digit NANP number → +1…
func normalizePhoneE164(raw string) (string, bool) {
	var b strings.Builder
	for _, c := range strings.TrimSpace(raw) {
		switch c {
		case ' ', '-', '.', '(', ')':
			// separator — drop
		default:
			b.WriteRune(c)
		}
	}
	s := b.String()
	plus := strings.HasPrefix(s, "+")
	digits := strings.TrimPrefix(s, "+")
	if digits == "" || !isAllDigits(digits) {
		return "", false
	}
	if plus {
		if len(digits) >= 8 && len(digits) <= 15 && digits[0] != '0' {
			return "+" + digits, true
		}
		return "", false
	}
	switch {
	case len(digits) == 10:
		return "+1" + digits, true
	case len(digits) == 11 && digits[0] == '1':
		return "+" + digits, true
	}
	return "", false
}

func isAllDigits(s string) bool {
	for _, c := range s {
		if c < '0' || c > '9' {
			return false
		}
	}
	return true
}

// parseIntakeFields parses the raise tool's `fields` argument. Accepts the structured form
// ([{key, required?, label?}]) and the shorthand ["email", "name"] (shorthand ⇒ required).
// Unknown keys are an error (closed set).
func parseIntakeFields(raw any) ([]IntakeField, error) {
	items, ok := raw.([]any)
	if !ok {
		return nil, fmt.Errorf("'fields' must be an array")
	}
	if len(items) == 0 {
		return nil, fmt.Errorf("'fields' must contain at least one field")
	}
	fields := make([]IntakeField, 0, len(items))
	for _, item := range items {
		switch v := item.(type) {
		case string:
			key := strings.TrimSpace(v)
			if !intakeFieldKeys[key] {
				return nil, fmt.Errorf("unknown intake field '%s' (expected name, email, or phone)", key)
			}
			fields = append(fields, IntakeField{Key: key, Required: true})
		case map[string]any:
			key := strings.TrimSpace(asString(v["key"]))
			if !intakeFieldKeys[key] {
				return nil, fmt.Errorf("unknown intake field '%s' (expected name, email, or phone)", key)
			}
			required := true
			if b, ok := v["required"].(bool); ok {
				required = b
			}
			fields = append(fields, IntakeField{Key: key, Required: required, Label: strings.TrimSpace(asString(v["label"]))})
		default:
			return nil, fmt.Errorf("invalid field entry")
		}
	}
	return fields, nil
}

// IdentityIntakeKind is the `identity_intake` Rich Interaction kind — structured
// name/email/phone lead capture (see spec/interactions/identity-intake.schema.json).
type IdentityIntakeKind struct{}

// Kind returns the wire kind id.
func (IdentityIntakeKind) Kind() string { return "identity_intake" }

// Capability returns the render capability that gates the rich form path.
func (IdentityIntakeKind) Capability() string { return identityFormCapability }

// ToolSchema returns the request_identity_intake raise tool's LLM-facing schema.
func (IdentityIntakeKind) ToolSchema() InteractionToolSchema {
	return InteractionToolSchema{
		Name: "request_identity_intake",
		Description: "Ask the visitor for their contact details (name, email, and/or phone) in a " +
			"channel-appropriate way. On channels that can render a form the visitor fills a " +
			"structured form; on text channels you will be told to collect the fields " +
			"conversationally. Always use this tool instead of free-forming a request for contact details.",
		Parameters: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"fields": map[string]any{
					"type":        "array",
					"minItems":    1,
					"description": "Which fields to collect, in order. Each entry is either a string (\"name\" | \"email\" | \"phone\") or an object { key, required?, label? }.",
					"items": map[string]any{
						"anyOf": []any{
							map[string]any{"type": "string", "enum": []any{"name", "email", "phone"}},
							map[string]any{
								"type": "object",
								"properties": map[string]any{
									"key":      map[string]any{"type": "string", "enum": []any{"name", "email", "phone"}},
									"required": map[string]any{"type": "boolean"},
									"label":    map[string]any{"type": "string"},
								},
								"required": []any{"key"},
							},
						},
					},
				},
				"reason": map[string]any{"type": "string", "description": "Why you need these details, phrased for the visitor (e.g. \"to send you the quote\")."},
			},
			"required": []any{"fields", "reason"},
		},
	}
}

// ParseRequest parses + canonicalizes the raise tool's arguments into the identity_intake spec.
func (k IdentityIntakeKind) ParseRequest(args map[string]any) (InteractionRequest, error) {
	fields, err := parseIntakeFields(args["fields"])
	if err != nil {
		return InteractionRequest{}, err
	}
	reason := strings.TrimSpace(asString(args["reason"]))
	if reason == "" {
		reason = "to help you better"
	}
	spec, err := json.Marshal(identityIntakeSpec{Fields: fields})
	if err != nil {
		return InteractionRequest{}, err
	}
	return InteractionRequest{Kind: k.Kind(), Spec: spec, Reason: reason}, nil
}

// Validate validates submitted values against the raised spec, returning the canonical
// (normalized) IntakeValues or the full list of per-field errors. A nil/empty spec ⇒
// format-only validation (a prior-turn fallback raise whose spec is gone).
func (IdentityIntakeKind) Validate(spec json.RawMessage, values any) (any, []InteractionFieldError) {
	var parsedSpec identityIntakeSpec
	if len(spec) > 0 {
		_ = json.Unmarshal(spec, &parsedSpec)
	}
	var parsed IntakeValues
	rawValues, err := json.Marshal(values)
	if err == nil {
		err = json.Unmarshal(rawValues, &parsed)
	}
	if err != nil {
		return nil, []InteractionFieldError{{Field: "values", Message: fmt.Sprintf("invalid values shape: %v", err)}}
	}
	if parsed.isEmpty() {
		return nil, []InteractionFieldError{{Field: "values", Message: "provide at least one of name/email/phone, or declined=true"}}
	}
	canonical, errs := validateIntake(parsedSpec.Fields, parsed)
	if len(errs) > 0 {
		return nil, errs
	}
	return canonical, nil
}

// FallbackDirective is the conversational-degradation directive for text-only channels.
func (IdentityIntakeKind) FallbackDirective(spec json.RawMessage, reason string) string {
	var parsedSpec identityIntakeSpec
	if len(spec) > 0 {
		_ = json.Unmarshal(spec, &parsedSpec)
	}
	keys := make([]string, 0, len(parsedSpec.Fields))
	for _, f := range parsedSpec.Fields {
		keys = append(keys, f.Key)
	}
	return fmt.Sprintf(
		"This visitor's channel cannot display a form. Collect the requested details (%s) "+
			"conversationally: ask for ONE field at a time, in the order given, naturally weaving in "+
			"the reason (%s). When you have the values, call the `submit_interaction` tool with kind "+
			"\"identity_intake\" and the values — it validates each field and will tell you if "+
			"something looks wrong so you can re-ask. If the visitor declines to share, call "+
			"`submit_interaction` with declined=true and continue helping them without the details.",
		strings.Join(keys, ", "), reason,
	)
}

// AttachEffect is the identity_intake host effect (the InteractionEffect seam): on a valid
// submit — rich OR conversational — stamp the captured identity onto the session metadata
// (userName / contactEmail / contactPhone), the same keys the pre-chat create path stashes
// and the OTP contact seam reads, so the captured email/phone become OTP-contactable.
// Mirrors the Rust attach_session_identity.
func (IdentityIntakeKind) AttachEffect(ctx context.Context, store SessionStore, sessionID string, values any) {
	var v IntakeValues
	raw, err := json.Marshal(values)
	if err != nil {
		return
	}
	if err := json.Unmarshal(raw, &v); err != nil {
		return
	}
	if v.isEmpty() {
		return
	}
	// A persistence error must not fail the already-produced turn — swallow it (the
	// contact simply doesn't attach), mirroring the sibling SetCurrentStep call site.
	_ = store.AttachSessionContact(ctx, sessionID, v.Name, v.Email, v.Phone)
}
