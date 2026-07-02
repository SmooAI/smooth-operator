package server

import (
	"context"
	"encoding/json"
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// sampleWorkflow is a 3-step workflow used across the workflow tests.
func sampleWorkflow() *ConversationWorkflow {
	return &ConversationWorkflow{
		Goal: "Qualify the lead and book a demo.",
		Steps: []ConversationWorkflowStep{
			{ID: "greet", Intent: "Greet the caller and confirm their name.", Criteria: "The caller's name is confirmed."},
			{ID: "qualify", Intent: "Understand the caller's use case.", Criteria: "The use case is captured."},
			{ID: "book", Intent: "Offer to book a demo.", Criteria: "A demo time is agreed."},
		},
	}
}

func TestParseAgentConfig(t *testing.T) {
	validWF := `"conversation_workflow":{"goal":"g","steps":[{"id":"a","intent":"i","criteria":"c"}]}`
	tests := []struct {
		name         string
		raw          string
		wantNil      bool
		wantPrompt   string
		wantWorkflow bool
		wantGreeting string
		wantTools    []string
	}{
		{name: "empty record -> nil", raw: ``, wantNil: true},
		{name: "not an object -> nil", raw: `"just a string"`, wantNil: true},
		{name: "malformed json -> nil", raw: `{not json`, wantNil: true},
		{name: "no usable fields -> nil", raw: `{"other":"x"}`, wantNil: true},
		{name: "instructions object prompt", raw: `{"instructions":{"prompt":"Be terse."}}`, wantPrompt: "Be terse."},
		{name: "instructions bare string", raw: `{"instructions":"You are Bob."}`, wantPrompt: "You are Bob."},
		{name: "instructions trimmed", raw: `{"instructions":{"prompt":"  hi  "}}`, wantPrompt: "hi"},
		{name: "instructions empty prompt -> nil", raw: `{"instructions":{"prompt":""}}`, wantNil: true},
		{name: "valid workflow", raw: `{` + validWF + `}`, wantWorkflow: true},
		{name: "camelCase workflow key", raw: `{"conversationWorkflow":{"goal":"g","steps":[{"id":"a","intent":"i","criteria":"c"}]}}`, wantWorkflow: true},
		{name: "broken workflow keeps valid instructions", raw: `{"instructions":"hi","conversation_workflow":{"goal":""}}`, wantPrompt: "hi", wantWorkflow: false},
		{name: "greeting + personality", raw: `{"greeting":"Hi there","personality":"warm"}`, wantGreeting: "Hi there"},
		{name: "tool_config allow-list", raw: `{"tool_config":["knowledge_search","fetch_url"]}`, wantTools: []string{"knowledge_search", "fetch_url"}},
		{name: "allowedTools camel", raw: `{"allowedTools":["a"]}`, wantTools: []string{"a"}},
		{name: "empty tool array -> nil field", raw: `{"tool_config":[]}`, wantNil: true},
		{name: "everything", raw: `{"instructions":"You are Bob.",` + validWF + `,"greeting":"Hi","personality":"warm","tool_config":["a"]}`, wantPrompt: "You are Bob.", wantWorkflow: true, wantGreeting: "Hi", wantTools: []string{"a"}},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := ParseAgentConfig(json.RawMessage(tt.raw))
			if tt.wantNil {
				if cfg != nil {
					t.Fatalf("expected nil config, got %+v", cfg)
				}
				return
			}
			if cfg == nil {
				t.Fatal("expected non-nil config, got nil")
			}
			if cfg.Instructions != tt.wantPrompt {
				t.Errorf("Instructions = %q, want %q", cfg.Instructions, tt.wantPrompt)
			}
			if (cfg.Workflow != nil) != tt.wantWorkflow {
				t.Errorf("Workflow present = %v, want %v", cfg.Workflow != nil, tt.wantWorkflow)
			}
			if cfg.Greeting != tt.wantGreeting {
				t.Errorf("Greeting = %q, want %q", cfg.Greeting, tt.wantGreeting)
			}
			if strings.Join(cfg.AllowedTools, ",") != strings.Join(tt.wantTools, ",") {
				t.Errorf("AllowedTools = %v, want %v", cfg.AllowedTools, tt.wantTools)
			}
		})
	}
}

func TestParseWorkflowTooManySteps(t *testing.T) {
	// 21 steps exceeds the max of 20 → degrade to nil.
	var b strings.Builder
	b.WriteString(`{"goal":"g","steps":[`)
	for i := 0; i < 21; i++ {
		if i > 0 {
			b.WriteByte(',')
		}
		b.WriteString(`{"id":"s`)
		b.WriteString(string(rune('a' + i%26)))
		b.WriteString(string(rune('0' + i/26)))
		b.WriteString(`","intent":"i","criteria":"c"}`)
	}
	b.WriteString(`]}`)
	if wf := parseWorkflow(json.RawMessage(b.String())); wf != nil {
		t.Fatalf("expected nil for 21-step workflow (max 20), got %d steps", len(wf.Steps))
	}
}

func TestResolveCurrentStep(t *testing.T) {
	wf := sampleWorkflow()
	tests := []struct {
		name    string
		pointer string
		wantID  string
	}{
		{name: "empty pointer -> first step", pointer: "", wantID: "greet"},
		{name: "known pointer -> that step", pointer: "qualify", wantID: "qualify"},
		{name: "unknown pointer -> first step", pointer: "nope", wantID: "greet"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := resolveCurrentStep(wf, tt.pointer)
			if got == nil || got.ID != tt.wantID {
				t.Fatalf("resolveCurrentStep(%q) = %v, want id %q", tt.pointer, got, tt.wantID)
			}
		})
	}
	if resolveCurrentStep(nil, "greet") != nil {
		t.Error("resolveCurrentStep(nil, ...) should be nil")
	}
}

func TestNextStep(t *testing.T) {
	wf := sampleWorkflow()
	// Sequential fallthrough: greet -> qualify -> book -> nil (terminal).
	if got := nextStep(wf, &wf.Steps[0]); got == nil || got.ID != "qualify" {
		t.Errorf("nextStep(greet) = %v, want qualify", got)
	}
	if got := nextStep(wf, &wf.Steps[1]); got == nil || got.ID != "book" {
		t.Errorf("nextStep(qualify) = %v, want book", got)
	}
	if got := nextStep(wf, &wf.Steps[2]); got != nil {
		t.Errorf("nextStep(book) = %v, want nil (terminal)", got)
	}

	// Explicit `next` jumps, overriding array order.
	jump := &ConversationWorkflow{
		Goal: "g",
		Steps: []ConversationWorkflowStep{
			{ID: "a", Intent: "i", Criteria: "c", Next: "c"},
			{ID: "b", Intent: "i", Criteria: "c"},
			{ID: "c", Intent: "i", Criteria: "c"},
		},
	}
	if got := nextStep(jump, &jump.Steps[0]); got == nil || got.ID != "c" {
		t.Errorf("nextStep(a with next=c) = %v, want c", got)
	}

	// Explicit `next` that doesn't resolve falls back to array order.
	dangling := &ConversationWorkflow{
		Goal:  "g",
		Steps: []ConversationWorkflowStep{{ID: "a", Intent: "i", Criteria: "c", Next: "ghost"}, {ID: "b", Intent: "i", Criteria: "c"}},
	}
	if got := nextStep(dangling, &dangling.Steps[0]); got == nil || got.ID != "b" {
		t.Errorf("nextStep(a with dangling next) = %v, want b (array fallback)", got)
	}
}

func TestRenderWorkflowPromptSection(t *testing.T) {
	wf := sampleWorkflow()

	// No workflow -> empty string (caller interpolates unconditionally).
	if got := renderWorkflowPromptSection(nil, ""); got != "" {
		t.Errorf("render(nil) = %q, want empty", got)
	}

	got := renderWorkflowPromptSection(wf, "qualify")
	for _, want := range []string{
		"<ConversationWorkflow>",
		"GOAL: Qualify the lead and book a demo.",
		"CURRENT STEP (2/3): qualify",
		"INTENT: Understand the caller's use case.",
		"CRITERIA: The use case is captured.",
		"</ConversationWorkflow>",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("rendered section missing %q\n---\n%s", want, got)
		}
	}

	// Empty pointer renders the first step at 1/N.
	if !strings.Contains(renderWorkflowPromptSection(wf, ""), "CURRENT STEP (1/3): greet") {
		t.Error("empty pointer should render the first step at 1/3")
	}
}

func TestAssembleSystemPrompt(t *testing.T) {
	wf := sampleWorkflow()
	base := "DEFAULT PERSONA"

	// nil config: base unchanged.
	if got := assembleSystemPrompt(base, nil, "", true); got != base {
		t.Errorf("nil config = %q, want base unchanged", got)
	}

	// Instructions AUGMENT the base (both present; instructions lead, base retained so
	// its grounding rules still apply).
	got := assembleSystemPrompt(base, &AgentConfig{Instructions: "You are Bob."}, "", true)
	if !strings.Contains(got, "<AgentInstructions>") || !strings.Contains(got, "You are Bob.") {
		t.Errorf("instructions missing / not wrapped: %q", got)
	}
	if !strings.Contains(got, base) {
		t.Errorf("base grounding rules should be retained alongside instructions: %q", got)
	}
	if strings.Index(got, "You are Bob.") > strings.Index(got, base) {
		t.Errorf("instructions should lead the base prompt: %q", got)
	}

	// Personality leads; greeting (first turn) + workflow sections included.
	got = assembleSystemPrompt(base, &AgentConfig{Personality: "warm", Greeting: "Hi!", Workflow: wf}, "greet", true)
	for _, want := range []string{"<Personality>", "warm", base, "<GreetingAwareness>", "Hi!", "<ConversationWorkflow>", "CURRENT STEP (1/3): greet"} {
		if !strings.Contains(got, want) {
			t.Errorf("assembled prompt missing %q\n%s", want, got)
		}
	}

	// Greeting is turn-1-only: absent on later turns; personality + workflow still present.
	got = assembleSystemPrompt(base, &AgentConfig{Personality: "warm", Greeting: "Hi!", Workflow: wf}, "greet", false)
	if strings.Contains(got, "<GreetingAwareness>") || strings.Contains(got, "Hi!") {
		t.Errorf("greeting must not appear after turn 1:\n%s", got)
	}
	if !strings.Contains(got, "<Personality>") || !strings.Contains(got, "<ConversationWorkflow>") {
		t.Errorf("non-greeting sections should still be present on later turns:\n%s", got)
	}

	// Workflow section reflects the current-step pointer.
	got = assembleSystemPrompt(base, &AgentConfig{Workflow: wf}, "book", false)
	if !strings.Contains(got, base) || !strings.Contains(got, "CURRENT STEP (3/3): book") {
		t.Errorf("expected base + book step, got %q", got)
	}
}

func TestFilterTools(t *testing.T) {
	tools := []core.Tool{stubTool("knowledge_search"), stubTool("fetch_url"), stubTool("notify_humans")}

	// nil config / empty allow-list -> unchanged.
	if got := filterTools(tools, nil); len(got) != 3 {
		t.Errorf("nil config should keep all tools, got %d", len(got))
	}
	if got := filterTools(tools, &AgentConfig{}); len(got) != 3 {
		t.Errorf("empty allow-list should keep all tools, got %d", len(got))
	}

	// Allow-list filters to the named tools.
	got := filterTools(tools, &AgentConfig{AllowedTools: []string{"fetch_url", "unknown"}})
	if len(got) != 1 || got[0].Name() != "fetch_url" {
		t.Errorf("allow-list filter = %v, want [fetch_url]", toolNames(got))
	}
}

func TestAdvanceStep(t *testing.T) {
	wf := sampleWorkflow()
	tests := []struct {
		name    string
		pointer string
		verdict WorkflowVerdict
		want    string
	}{
		{name: "yes advances", pointer: "greet", verdict: VerdictYes, want: "qualify"},
		{name: "no stays", pointer: "greet", verdict: VerdictNo, want: "greet"},
		{name: "maybe stays", pointer: "greet", verdict: VerdictMaybe, want: "greet"},
		{name: "skipped stays", pointer: "greet", verdict: VerdictSkipped, want: "greet"},
		{name: "yes on terminal stays", pointer: "book", verdict: VerdictYes, want: "book"},
		{name: "empty pointer + yes initializes then advances", pointer: "", verdict: VerdictYes, want: "qualify"},
		{name: "empty pointer + no initializes to first", pointer: "", verdict: VerdictNo, want: "greet"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := advanceStep(wf, tt.pointer, tt.verdict); got != tt.want {
				t.Errorf("advanceStep(%q, %s) = %q, want %q", tt.pointer, tt.verdict, got, tt.want)
			}
		})
	}
	if got := advanceStep(nil, "greet", VerdictYes); got != "" {
		t.Errorf("advanceStep(nil workflow) = %q, want empty", got)
	}
}

func TestStaticAgentConfigResolver(t *testing.T) {
	resolver := NewStaticAgentConfigResolver(map[string]*AgentConfig{
		"agent-a": {Instructions: "A persona"},
		"agent-b": {Instructions: "B persona", Workflow: sampleWorkflow()},
	})

	// Unknown agent -> nil (no config).
	cfg, err := resolver.Resolve(t.Context(), "unknown")
	if err != nil {
		t.Fatalf("Resolve error: %v", err)
	}
	if cfg != nil {
		t.Errorf("unknown agent should resolve to nil, got %+v", cfg)
	}

	// Per-agent isolation: two agents, distinct configs.
	a, _ := resolver.Resolve(t.Context(), "agent-a")
	b, _ := resolver.Resolve(t.Context(), "agent-b")
	if a == nil || a.Instructions != "A persona" || a.Workflow != nil {
		t.Errorf("agent-a config wrong: %+v", a)
	}
	if b == nil || b.Instructions != "B persona" || b.Workflow == nil {
		t.Errorf("agent-b config wrong: %+v", b)
	}
}

// stubTool is a minimal core.Tool for filter tests (never executed).
type stubTool string

func (s stubTool) Name() string                                            { return string(s) }
func (s stubTool) Description() string                                     { return "" }
func (s stubTool) Parameters() map[string]any                              { return nil }
func (s stubTool) Execute(context.Context, map[string]any) (string, error) { return "", nil }

func toolNames(tools []core.Tool) []string {
	names := make([]string, len(tools))
	for i, t := range tools {
		names[i] = t.Name()
	}
	return names
}
