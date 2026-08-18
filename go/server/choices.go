package server

import (
	"encoding/json"
	"fmt"
	"strings"
	"unicode/utf8"
)

// Choices — a structured multiple-choice ask modeled on Claude Code's
// AskUserQuestion: the reference Rich Interaction kind (the Go port of
// rust/smooth-operator/src/choices.rs). The agent asks 1–4 short questions, each
// with 2–4 labeled options; the turn parks until the visitor picks. Every question
// also carries an implicit free-text "Other" escape hatch, so the visitor can answer
// outside the enumerated options.
//
//   - On a channel that declared the `choice_chips` capability, the agent's
//     request_choices tool parks the turn and the server emits interaction_required;
//     the client's chip/menu card resumes with a submit_interaction action.
//   - On a text-only channel the same raise degrades to a conversational directive
//     enumerating the questions + options.
//
// Both paths validate through validateChoices — one implementation, one behavior —
// and resume with the same structured payload. Validated against the shared choices
// fixtures (spec/conformance/fixtures.json) in choices_test.go.

// headerMaxChars is the max length of a question's short header label (chip/tab caption).
const headerMaxChars = 12

// choiceChipsCapability gates the rich (parked card) path for the choices kind.
const choiceChipsCapability = "choice_chips"

// ChoiceOption is one selectable option in a question.
type ChoiceOption struct {
	// Label is the option's label — the value the visitor submits.
	Label string `json:"label"`
	// Description is a short human-readable gloss shown under/next to the label.
	Description string `json:"description,omitempty"`
}

// ChoiceQuestion is one question in a choices raise.
type ChoiceQuestion struct {
	// Question is the prompt shown to the visitor.
	Question string `json:"question"`
	// Header is a short label (≤headerMaxChars) — the answer key and the chip/tab
	// caption. Unique within a raise.
	Header string `json:"header"`
	// Options are the 2–4 enumerated options. An implicit free-text "Other" is always available too.
	Options []ChoiceOption `json:"options"`
	// MultiSelect reports whether the visitor may pick more than one option (default false).
	MultiSelect bool `json:"multiSelect,omitempty"`
}

// choicesSpec is the render spec carried on interaction_required for kind choices.
type choicesSpec struct {
	Questions []ChoiceQuestion `json:"questions"`
}

// ChoiceAnswer is the visitor's answer to one question, submitted via submit_interaction.
type ChoiceAnswer struct {
	// Header identifies which question this answers — matches the spec question's Header.
	Header string `json:"header"`
	// Options are the selected option label(s). One for single-select; empty when the
	// visitor only used the free-text "Other" escape hatch.
	Options []string `json:"options,omitempty"`
	// Other is the free-text "Other" answer, when the visitor answered outside the
	// enumerated options. Blank ⇒ omitted.
	Other string `json:"other,omitempty"`
}

// selectionCount is the total picks the visitor made (selected labels + one for a
// non-blank "Other").
func (a ChoiceAnswer) selectionCount() int {
	n := len(a.Options)
	if a.Other != "" {
		n++
	}
	return n
}

// ChoiceValues is the visitor's submitted answers.
type ChoiceValues struct {
	Answers []ChoiceAnswer `json:"answers"`
}

// validateChoices validates submitted values against the raised questions, returning
// the normalized ChoiceValues or the full list of per-question errors.
//
// Rules (mirror choices.rs):
//   - every question must be answered (a selection or a non-blank "Other");
//   - each selected label must be one of that question's option labels;
//   - single-select: exactly one pick (one label XOR "Other"); multi-select: ≥1;
//   - a blank/whitespace "Other" is treated as absent; labels are trimmed.
//
// When questions is empty (a prior-turn fallback raise whose spec is gone), validation
// degrades to format-only: any answer with at least one pick is accepted as-is.
// Returns every failed question (not just the first) so a card can annotate all of
// them in one round-trip.
func validateChoices(questions []ChoiceQuestion, values ChoiceValues) (ChoiceValues, []InteractionFieldError) {
	// Normalize the raw answers first (trim labels + "Other", drop blanks).
	normalized := make([]ChoiceAnswer, 0, len(values.Answers))
	for _, a := range values.Answers {
		opts := make([]string, 0, len(a.Options))
		for _, o := range a.Options {
			if t := strings.TrimSpace(o); t != "" {
				opts = append(opts, t)
			}
		}
		normalized = append(normalized, ChoiceAnswer{
			Header:  strings.TrimSpace(a.Header),
			Options: opts,
			Other:   strings.TrimSpace(a.Other),
		})
	}

	// Format-only path: no spec to check membership/required-ness against.
	if len(questions) == 0 {
		var errs []InteractionFieldError
		for _, a := range normalized {
			if a.selectionCount() == 0 {
				errs = append(errs, InteractionFieldError{Field: a.Header, Message: "select an option or provide an 'other' answer"})
			}
		}
		if len(normalized) == 0 {
			errs = append(errs, InteractionFieldError{Field: "answers", Message: "provide an answer for each question, or declined=true"})
		}
		if len(errs) > 0 {
			return ChoiceValues{}, errs
		}
		return ChoiceValues{Answers: normalized}, nil
	}

	var errs []InteractionFieldError
	out := make([]ChoiceAnswer, 0, len(questions))

	for _, q := range questions {
		answer, found := findAnswer(normalized, q.Header)
		if !found {
			errs = append(errs, InteractionFieldError{Field: q.Header, Message: "this question must be answered"})
			continue
		}

		// Every selected label must be one of the enumerated options.
		badLabel := false
		for _, label := range answer.Options {
			if !hasOption(q.Options, label) {
				badLabel = true
				errs = append(errs, InteractionFieldError{Field: q.Header, Message: fmt.Sprintf("'%s' is not one of the offered options", label)})
			}
		}

		count := answer.selectionCount()
		if count == 0 {
			errs = append(errs, InteractionFieldError{Field: q.Header, Message: "select an option or provide an 'other' answer"})
		} else if !q.MultiSelect && count > 1 {
			errs = append(errs, InteractionFieldError{Field: q.Header, Message: "this question takes a single answer"})
		}

		if !badLabel {
			out = append(out, answer)
		}
	}

	if len(errs) > 0 {
		return ChoiceValues{}, errs
	}
	return ChoiceValues{Answers: out}, nil
}

func findAnswer(answers []ChoiceAnswer, header string) (ChoiceAnswer, bool) {
	for _, a := range answers {
		if a.Header == header {
			return a, true
		}
	}
	return ChoiceAnswer{}, false
}

func hasOption(options []ChoiceOption, label string) bool {
	for _, o := range options {
		if o.Label == label {
			return true
		}
	}
	return false
}

// parseQuestions parses the raise tool's `questions` argument into validated
// ChoiceQuestions. Enforces the LLM-facing contract: 1–4 questions, each with a
// non-empty prompt, a non-empty header ≤12 chars (unique within the raise), and 2–4
// options with non-empty labels.
func parseQuestions(raw any) ([]ChoiceQuestion, error) {
	items, ok := raw.([]any)
	if !ok {
		return nil, fmt.Errorf("'questions' must be an array")
	}
	if len(items) < 1 || len(items) > 4 {
		return nil, fmt.Errorf("'questions' must contain between 1 and 4 questions")
	}
	questions := make([]ChoiceQuestion, 0, len(items))
	seen := map[string]bool{}
	for _, item := range items {
		obj, ok := item.(map[string]any)
		if !ok {
			return nil, fmt.Errorf("each question must be an object")
		}
		question := strings.TrimSpace(asString(obj["question"]))
		if question == "" {
			return nil, fmt.Errorf("each question needs a non-empty 'question'")
		}
		header := strings.TrimSpace(asString(obj["header"]))
		if header == "" {
			return nil, fmt.Errorf("each question needs a non-empty 'header'")
		}
		if utf8.RuneCountInString(header) > headerMaxChars {
			return nil, fmt.Errorf("header '%s' is too long (max %d characters)", header, headerMaxChars)
		}
		if seen[header] {
			return nil, fmt.Errorf("duplicate question header '%s'", header)
		}
		seen[header] = true

		rawOptions, ok := obj["options"].([]any)
		if !ok {
			return nil, fmt.Errorf("question '%s' needs an 'options' array", header)
		}
		if len(rawOptions) < 2 || len(rawOptions) > 4 {
			return nil, fmt.Errorf("question '%s' must offer between 2 and 4 options", header)
		}
		options := make([]ChoiceOption, 0, len(rawOptions))
		for _, opt := range rawOptions {
			// Accept the object form { label, description? } and the bare-string shorthand.
			var option ChoiceOption
			switch v := opt.(type) {
			case string:
				option = ChoiceOption{Label: strings.TrimSpace(v)}
			case map[string]any:
				option = ChoiceOption{
					Label:       strings.TrimSpace(asString(v["label"])),
					Description: strings.TrimSpace(asString(v["description"])),
				}
			default:
				return nil, fmt.Errorf("invalid option entry in '%s'", header)
			}
			if option.Label == "" {
				return nil, fmt.Errorf("an option in '%s' has an empty label", header)
			}
			options = append(options, option)
		}

		multi, _ := obj["multiSelect"].(bool)
		questions = append(questions, ChoiceQuestion{
			Question:    question,
			Header:      header,
			Options:     options,
			MultiSelect: multi,
		})
	}
	return questions, nil
}

// asString coerces a JSON-decoded value to a string ("" for anything non-string).
func asString(v any) string {
	s, _ := v.(string)
	return s
}

// ChoicesKind is the `choices` Rich Interaction kind — a structured multiple-choice
// ask modeled on AskUserQuestion (see spec/interactions/choices.schema.json).
type ChoicesKind struct{}

// Kind returns the wire kind id.
func (ChoicesKind) Kind() string { return "choices" }

// Capability returns the render capability that gates the rich card path.
func (ChoicesKind) Capability() string { return choiceChipsCapability }

// ToolSchema returns the request_choices raise tool's LLM-facing schema.
func (ChoicesKind) ToolSchema() InteractionToolSchema {
	return InteractionToolSchema{
		Name: "request_choices",
		Description: "Ask the visitor a structured multiple-choice question (1–4 questions, each " +
			"with 2–4 labeled options) and wait for their pick. On channels that can render " +
			"chips/menus the visitor taps an option; on text channels you will be told to " +
			"enumerate the options and accept a natural-language answer. An implicit free-text " +
			"\"Other\" is always available, so use this whenever the answer is likely (but not " +
			"certainly) one of a small set — never free-form the menu yourself.",
		Parameters: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"questions": map[string]any{
					"type":        "array",
					"minItems":    1,
					"maxItems":    4,
					"description": "The questions to ask, in order (1–4).",
					"items": map[string]any{
						"type": "object",
						"properties": map[string]any{
							"question": map[string]any{"type": "string", "description": "The question prompt shown to the visitor."},
							"header":   map[string]any{"type": "string", "maxLength": headerMaxChars, "description": "A short label (≤12 chars), unique within the raise. Used as the answer key and the chip/tab caption."},
							"options": map[string]any{
								"type":        "array",
								"minItems":    2,
								"maxItems":    4,
								"description": "The 2–4 options to offer. A free-text 'Other' is always available in addition.",
								"items": map[string]any{
									"type": "object",
									"properties": map[string]any{
										"label":       map[string]any{"type": "string", "description": "The option label (the value submitted)."},
										"description": map[string]any{"type": "string", "description": "A short gloss for the option."},
									},
									"required": []any{"label"},
								},
							},
							"multiSelect": map[string]any{"type": "boolean", "description": "Allow selecting more than one option (default false)."},
						},
						"required": []any{"question", "header", "options"},
					},
				},
				"reason": map[string]any{"type": "string", "description": "Why you're asking, phrased for the visitor (e.g. \"to route you to the right team\")."},
			},
			"required": []any{"questions", "reason"},
		},
	}
}

// ParseRequest parses + canonicalizes the raise tool's arguments into the choices spec.
func (k ChoicesKind) ParseRequest(args map[string]any) (InteractionRequest, error) {
	questions, err := parseQuestions(args["questions"])
	if err != nil {
		return InteractionRequest{}, err
	}
	reason := strings.TrimSpace(asString(args["reason"]))
	if reason == "" {
		reason = "to help you better"
	}
	spec, err := json.Marshal(choicesSpec{Questions: questions})
	if err != nil {
		return InteractionRequest{}, err
	}
	return InteractionRequest{Kind: k.Kind(), Spec: spec, Reason: reason}, nil
}

// Validate validates submitted values against the raised spec, returning the canonical
// (normalized) values or the full list of per-question errors.
func (ChoicesKind) Validate(spec json.RawMessage, values any) (any, []InteractionFieldError) {
	var parsedSpec choicesSpec
	if len(spec) > 0 {
		// A malformed spec degrades to format-only (empty questions) rather than erroring.
		_ = json.Unmarshal(spec, &parsedSpec)
	}
	// Round-trip the decoded values (a map from the frame) through JSON into the typed shape.
	var parsedValues ChoiceValues
	rawValues, err := json.Marshal(values)
	if err == nil {
		err = json.Unmarshal(rawValues, &parsedValues)
	}
	if err != nil {
		return nil, []InteractionFieldError{{Field: "values", Message: fmt.Sprintf("invalid values shape: %v", err)}}
	}
	canonical, errs := validateChoices(parsedSpec.Questions, parsedValues)
	if len(errs) > 0 {
		return nil, errs
	}
	return canonical, nil
}

// FallbackDirective is the conversational-degradation directive for text-only channels.
func (ChoicesKind) FallbackDirective(spec json.RawMessage, reason string) string {
	var parsedSpec choicesSpec
	if len(spec) > 0 {
		_ = json.Unmarshal(spec, &parsedSpec)
	}
	lines := make([]string, 0, len(parsedSpec.Questions))
	for _, q := range parsedSpec.Questions {
		labels := make([]string, 0, len(q.Options))
		for _, o := range q.Options {
			labels = append(labels, o.Label)
		}
		multi := ""
		if q.MultiSelect {
			multi = " (choose one or more)"
		}
		lines = append(lines, fmt.Sprintf("- [%s] %s Options: %s%s.", q.Header, q.Question, strings.Join(labels, ", "), multi))
	}
	return fmt.Sprintf(
		"This visitor's channel cannot display choice chips. Ask the following question(s) "+
			"conversationally, naturally weaving in the reason (%s), and read out each option so "+
			"the visitor can pick:\n%s\nThe visitor may also answer with something not listed "+
			"(that's fine — capture it as their 'other' answer). Collect their pick(s) and continue "+
			"the conversation using the answers; one entry per question { header, options: [chosen "+
			"label(s)], other?: \"their free-text answer\" }. If a pick isn't one you offered, re-ask. "+
			"If the visitor declines to choose, continue helping them without it.",
		reason, strings.Join(lines, "\n"),
	)
}
