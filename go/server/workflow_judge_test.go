package server

import (
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

func TestParseVerdict(t *testing.T) {
	tests := []struct {
		content string
		want    WorkflowVerdict
	}{
		{content: "yes", want: VerdictYes},
		{content: "YES", want: VerdictYes},
		{content: "yes.", want: VerdictYes},
		{content: "Verdict: yes", want: VerdictYes},
		{content: "no", want: VerdictNo},
		{content: "No, the criteria are not met.", want: VerdictNo},
		{content: "maybe", want: VerdictMaybe},
		{content: "maybe, partial progress", want: VerdictMaybe},
		{content: "", want: VerdictSkipped},
		{content: "I am not sure how to answer", want: VerdictSkipped}, // "not" is not the token "no" — no false positive
		{content: "unclear rambling", want: VerdictSkipped},
	}
	for _, tt := range tests {
		t.Run(tt.content, func(t *testing.T) {
			if got := parseVerdict(tt.content); got != tt.want {
				t.Errorf("parseVerdict(%q) = %s, want %s", tt.content, got, tt.want)
			}
		})
	}
}

func TestJudgeWorkflowStepEarlyOuts(t *testing.T) {
	wf := sampleWorkflow()

	// nil client -> skipped, no call.
	if v := judgeWorkflowStep(t.Context(), nil, "", wf, "greet", "hi", "reply"); v != VerdictSkipped {
		t.Errorf("nil client = %s, want skipped", v)
	}

	// nil workflow -> skipped.
	mock := core.NewMockLlmProvider()
	if v := judgeWorkflowStep(t.Context(), mock, "", nil, "greet", "hi", "reply"); v != VerdictSkipped {
		t.Errorf("nil workflow = %s, want skipped", v)
	}
	if mock.CallCount() != 0 {
		t.Errorf("nil workflow should not call the judge, got %d calls", mock.CallCount())
	}

	// empty reply -> skipped, no call (nothing to judge).
	mock2 := core.NewMockLlmProvider().PushText("yes")
	if v := judgeWorkflowStep(t.Context(), mock2, "", wf, "greet", "hi", "   "); v != VerdictSkipped {
		t.Errorf("empty reply = %s, want skipped", v)
	}
	if mock2.CallCount() != 0 {
		t.Errorf("empty reply should not call the judge, got %d calls", mock2.CallCount())
	}
}

func TestJudgeWorkflowStepVerdicts(t *testing.T) {
	wf := sampleWorkflow()

	for _, tt := range []struct {
		reply string
		want  WorkflowVerdict
	}{
		{reply: "yes", want: VerdictYes},
		{reply: "no", want: VerdictNo},
		{reply: "maybe", want: VerdictMaybe},
	} {
		mock := core.NewMockLlmProvider().PushText(tt.reply)
		if v := judgeWorkflowStep(t.Context(), mock, "", wf, "greet", "hi there", "Nice to meet you, Alice."); v != tt.want {
			t.Errorf("judge reply %q -> %s, want %s", tt.reply, v, tt.want)
		}
	}
}

func TestJudgeWorkflowStepErrorTolerant(t *testing.T) {
	wf := sampleWorkflow()
	// A judge model error must NOT freeze the conversation -> skipped (stay on step).
	mock := core.NewMockLlmProvider().PushError("gateway 503")
	if v := judgeWorkflowStep(t.Context(), mock, "", wf, "greet", "hi", "some reply"); v != VerdictSkipped {
		t.Errorf("judge error = %s, want skipped (fail-safe)", v)
	}
}

func TestJudgeWorkflowStepPromptContainsStep(t *testing.T) {
	wf := sampleWorkflow()
	mock := core.NewMockLlmProvider().PushText("no")
	_ = judgeWorkflowStep(t.Context(), mock, "", wf, "qualify", "what do you do?", "We build agents.")

	call, ok := mock.LastCall()
	if !ok {
		t.Fatal("expected a judge call")
	}
	var joined string
	for _, m := range call.Messages {
		joined += m.Content + "\n"
	}
	for _, want := range []string{"Understand the caller's use case.", "The use case is captured.", "what do you do?", "We build agents."} {
		if !strings.Contains(joined, want) {
			t.Errorf("judge prompt missing %q", want)
		}
	}
	// Deterministic judge: temperature 0.
	if call.Temperature != 0 {
		t.Errorf("judge temperature = %v, want 0", call.Temperature)
	}
}
