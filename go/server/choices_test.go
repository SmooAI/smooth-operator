package server

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// Unit tests for the choices Rich Interaction kind — the Go port of the Rust
// rust/smooth-operator/src/choices.rs tests, plus validation against the shared
// conformance fixtures (spec/conformance/fixtures.json), so the Go validator agrees
// with the Rust reference on the same wire shapes.

func opt(label string) ChoiceOption { return ChoiceOption{Label: label} }

func question(header string, labels []string, multi bool) ChoiceQuestion {
	opts := make([]ChoiceOption, len(labels))
	for i, l := range labels {
		opts[i] = opt(l)
	}
	return ChoiceQuestion{Question: header + "?", Header: header, Options: opts, MultiSelect: multi}
}

func answer(header string, options []string, other string) ChoiceAnswer {
	return ChoiceAnswer{Header: header, Options: options, Other: other}
}

func TestValidateChoicesValidSingleSelectNormalizes(t *testing.T) {
	qs := []ChoiceQuestion{question("Plan", []string{"Basic", "Pro"}, false)}
	vals := ChoiceValues{Answers: []ChoiceAnswer{answer("Plan", []string{"  Pro  "}, "")}}
	out, errs := validateChoices(qs, vals)
	if errs != nil {
		t.Fatalf("expected valid, got errors: %v", errs)
	}
	if len(out.Answers) != 1 || len(out.Answers[0].Options) != 1 || out.Answers[0].Options[0] != "Pro" {
		t.Fatalf("expected normalized [Pro], got %+v", out.Answers)
	}
	if out.Answers[0].Other != "" {
		t.Fatalf("expected no other, got %q", out.Answers[0].Other)
	}
}

func TestValidateChoicesMultiSelectKeepsAllPicks(t *testing.T) {
	qs := []ChoiceQuestion{question("Topics", []string{"Sales", "Support", "Billing"}, true)}
	vals := ChoiceValues{Answers: []ChoiceAnswer{answer("Topics", []string{"Sales", "Billing"}, "")}}
	out, errs := validateChoices(qs, vals)
	if errs != nil {
		t.Fatalf("expected valid, got errors: %v", errs)
	}
	if strings.Join(out.Answers[0].Options, ",") != "Sales,Billing" {
		t.Fatalf("expected [Sales Billing], got %v", out.Answers[0].Options)
	}
}

func TestValidateChoicesOtherEscapeHatchAccepted(t *testing.T) {
	qs := []ChoiceQuestion{question("Plan", []string{"Basic", "Pro"}, false)}
	vals := ChoiceValues{Answers: []ChoiceAnswer{answer("Plan", nil, "  Enterprise, actually  ")}}
	out, errs := validateChoices(qs, vals)
	if errs != nil {
		t.Fatalf("expected valid, got errors: %v", errs)
	}
	if len(out.Answers[0].Options) != 0 {
		t.Fatalf("expected no option picks, got %v", out.Answers[0].Options)
	}
	if out.Answers[0].Other != "Enterprise, actually" {
		t.Fatalf("expected trimmed other, got %q", out.Answers[0].Other)
	}
}

func TestValidateChoicesUnknownLabelIsFieldError(t *testing.T) {
	qs := []ChoiceQuestion{question("Plan", []string{"Basic", "Pro"}, false)}
	vals := ChoiceValues{Answers: []ChoiceAnswer{answer("Plan", []string{"Platinum"}, "")}}
	_, errs := validateChoices(qs, vals)
	if len(errs) != 1 {
		t.Fatalf("expected 1 error, got %v", errs)
	}
	if errs[0].Field != "Plan" || !strings.Contains(errs[0].Message, "not one of the offered") {
		t.Fatalf("unexpected error: %+v", errs[0])
	}
}

func TestValidateChoicesSingleSelectRejectsMultiplePicks(t *testing.T) {
	qs := []ChoiceQuestion{question("Plan", []string{"Basic", "Pro"}, false)}
	vals := ChoiceValues{Answers: []ChoiceAnswer{answer("Plan", []string{"Basic", "Pro"}, "")}}
	_, errs := validateChoices(qs, vals)
	if !anyErrContains(errs, "single answer") {
		t.Fatalf("expected single-answer error, got %v", errs)
	}
}

func TestValidateChoicesUnansweredQuestionRequired(t *testing.T) {
	qs := []ChoiceQuestion{
		question("Plan", []string{"Basic", "Pro"}, false),
		question("Size", []string{"S", "M"}, false),
	}
	vals := ChoiceValues{Answers: []ChoiceAnswer{answer("Plan", []string{"Pro"}, "")}}
	_, errs := validateChoices(qs, vals)
	if len(errs) != 1 || errs[0].Field != "Size" || !strings.Contains(errs[0].Message, "must be answered") {
		t.Fatalf("expected Size must-be-answered, got %v", errs)
	}
}

func TestValidateChoicesEmptyAnswerNeedsPickOrOther(t *testing.T) {
	qs := []ChoiceQuestion{question("Plan", []string{"Basic", "Pro"}, false)}
	vals := ChoiceValues{Answers: []ChoiceAnswer{answer("Plan", nil, "")}}
	_, errs := validateChoices(qs, vals)
	if !anyErrContains(errs, "select an option") {
		t.Fatalf("expected select-an-option error, got %v", errs)
	}
}

func TestValidateChoicesFormatOnlyWhenSpecGone(t *testing.T) {
	// No questions (prior-turn fallback raise) → format-only: a pick is accepted as-is.
	out, errs := validateChoices(nil, ChoiceValues{Answers: []ChoiceAnswer{answer("Plan", []string{"Anything"}, "")}})
	if errs != nil {
		t.Fatalf("format-only should accept any pick, got %v", errs)
	}
	if out.Answers[0].Options[0] != "Anything" {
		t.Fatalf("format-only should keep the pick, got %v", out.Answers)
	}
	// …but an empty answer set still errors.
	_, errs = validateChoices(nil, ChoiceValues{})
	if !anyErrContains(errs, "provide an answer") {
		t.Fatalf("expected empty-answers error, got %v", errs)
	}
}

func TestParseQuestionsEnforcesContract(t *testing.T) {
	// Happy path with shorthand string options.
	qs, err := parseQuestions([]any{
		map[string]any{"question": "Which plan?", "header": "Plan", "options": []any{"Basic", "Pro"}},
	})
	if err != nil {
		t.Fatalf("valid parse failed: %v", err)
	}
	if len(qs) != 1 || qs[0].Options[0].Label != "Basic" || qs[0].MultiSelect {
		t.Fatalf("unexpected parse result: %+v", qs)
	}

	// Too many questions.
	tooMany := make([]any, 5)
	for i := range tooMany {
		tooMany[i] = map[string]any{"question": "q", "header": string(rune('A' + i)), "options": []any{"a", "b"}}
	}
	if _, err := parseQuestions(tooMany); err == nil {
		t.Fatal("expected error for >4 questions")
	}
	// Too few options.
	if _, err := parseQuestions([]any{map[string]any{"question": "q", "header": "H", "options": []any{"only"}}}); err == nil {
		t.Fatal("expected error for <2 options")
	}
	// Header too long.
	if _, err := parseQuestions([]any{map[string]any{"question": "q", "header": "ThisHeaderIsWayTooLong", "options": []any{"a", "b"}}}); err == nil {
		t.Fatal("expected error for long header")
	}
	// Duplicate headers.
	if _, err := parseQuestions([]any{
		map[string]any{"question": "q1", "header": "H", "options": []any{"a", "b"}},
		map[string]any{"question": "q2", "header": "H", "options": []any{"a", "b"}},
	}); err == nil {
		t.Fatal("expected error for duplicate headers")
	}
}

func TestChoicesKindWiresReferenceSurface(t *testing.T) {
	k := ChoicesKind{}
	if k.Kind() != "choices" || k.Capability() != "choice_chips" || k.ToolSchema().Name != "request_choices" {
		t.Fatalf("unexpected kind surface: %s/%s/%s", k.Kind(), k.Capability(), k.ToolSchema().Name)
	}

	req, err := k.ParseRequest(map[string]any{
		"questions": []any{map[string]any{"question": "Which plan interests you?", "header": "Plan",
			"options": []any{map[string]any{"label": "Basic"}, map[string]any{"label": "Pro"}}}},
		"reason": "to route you",
	})
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if req.Kind != "choices" || req.Reason != "to route you" {
		t.Fatalf("unexpected request: %+v", req)
	}
	var spec choicesSpec
	if err := json.Unmarshal(req.Spec, &spec); err != nil || spec.Questions[0].Header != "Plan" {
		t.Fatalf("unexpected spec: %s (%v)", req.Spec, err)
	}

	// The validator, through the kind, produces the canonical values.
	canonical, errs := k.Validate(req.Spec, map[string]any{"answers": []any{map[string]any{"header": "Plan", "options": []any{"Pro"}}}})
	if errs != nil {
		t.Fatalf("expected valid submit, got %v", errs)
	}
	cv := canonical.(ChoiceValues)
	if cv.Answers[0].Options[0] != "Pro" {
		t.Fatalf("unexpected canonical values: %+v", cv)
	}

	// The fallback directive enumerates the options.
	directive := k.FallbackDirective(req.Spec, "to route you")
	if !strings.Contains(directive, "Basic, Pro") {
		t.Fatalf("fallback directive missing enumerated options: %s", directive)
	}
}

// TestChoicesFixturesValidate cross-checks the Go kind against the shared conformance
// fixtures the Rust reference validates against: the choices_values submitted against the
// choices_spec must validate to the choices_payload's normalized answers.
func TestChoicesFixturesValidate(t *testing.T) {
	fixtures := loadFixtures(t)
	spec := fixtureInstance(t, fixtures, "choices_spec")
	values := fixtureInstance(t, fixtures, "choices_values")
	payload := fixtureInstance(t, fixtures, "choices_payload")

	specJSON, err := json.Marshal(spec)
	if err != nil {
		t.Fatalf("marshal spec: %v", err)
	}

	canonical, errs := ChoicesKind{}.Validate(specJSON, values)
	if errs != nil {
		t.Fatalf("shared choices_values should validate against choices_spec, got %v", errs)
	}

	// The canonical result must match the fixture payload's `values` (jsonEqual
	// normalizes both through JSON, so the struct tags line up with the wire shape).
	if !jsonEqual(canonical, payload["values"]) {
		got, _ := json.Marshal(canonical)
		want, _ := json.Marshal(payload["values"])
		t.Fatalf("canonical values != fixture payload values\n got: %s\nwant: %s", got, want)
	}
	if payload["status"] != "submitted" {
		t.Fatalf("fixture payload status = %v, want submitted", payload["status"])
	}
}

func loadFixtures(t *testing.T) map[string]any {
	t.Helper()
	path := filepath.Join("..", "..", "spec", "conformance", "fixtures.json")
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read fixtures: %v", err)
	}
	var f map[string]any
	if err := json.Unmarshal(data, &f); err != nil {
		t.Fatalf("parse fixtures: %v", err)
	}
	return f
}

func fixtureInstance(t *testing.T, fixtures map[string]any, key string) map[string]any {
	t.Helper()
	entry, ok := fixtures[key].(map[string]any)
	if !ok {
		t.Fatalf("fixture %q missing", key)
	}
	inst, ok := entry["instance"].(map[string]any)
	if !ok {
		t.Fatalf("fixture %q has no instance", key)
	}
	return inst
}

func anyErrContains(errs []InteractionFieldError, sub string) bool {
	for _, e := range errs {
		if strings.Contains(e.Message, sub) {
			return true
		}
	}
	return false
}
