package server

import (
	"context"
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// systemPromptOf returns the concatenated system-role message content the engine sent on
// the first (turn) LLM call — the assembled system prompt the caller passed the runner.
func systemPromptOf(t *testing.T, mock *core.MockLlmProvider) string {
	t.Helper()
	calls := mock.Calls()
	if len(calls) == 0 {
		t.Fatal("expected at least one LLM call")
	}
	var sb strings.Builder
	for _, m := range calls[0].Messages {
		if m.Role == "system" {
			sb.WriteString(m.Content)
			sb.WriteString("\n")
		}
	}
	return sb.String()
}

// TestTurnUsesAgentInstructions asserts the assembled per-agent prompt (instructions +
// retained base) reaches the engine as the system prompt.
func TestTurnUsesAgentInstructions(t *testing.T) {
	store := NewInMemorySessionStore()
	session, _ := store.CreateSession(context.Background(), "agent-x", "Alice", "a@example.com")
	mock := core.NewMockLlmProvider().PushText("Hello from Bob.")

	prompt := assembleSystemPrompt(defaultSystemPrompt, &AgentConfig{Instructions: "You are Bob, a laconic support agent."}, "", true)
	runner := NewTurnRunner(mock, store, prompt, nil, nil, nil, nil, nil, "", "", nil)
	if _, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", "hi", func(map[string]any) {}); err != nil {
		t.Fatalf("run: %v", err)
	}

	got := systemPromptOf(t, mock)
	if !strings.Contains(got, "You are Bob, a laconic support agent.") {
		t.Errorf("system prompt missing agent instructions:\n%s", got)
	}
	if !strings.Contains(got, defaultSystemPrompt) {
		t.Errorf("base grounding rules should be retained alongside instructions:\n%s", got)
	}
}

// TestTurnDefaultPromptWithoutConfig asserts a nil config leaves the built-in default
// persona in place (behavior unchanged).
func TestTurnDefaultPromptWithoutConfig(t *testing.T) {
	store := NewInMemorySessionStore()
	session, _ := store.CreateSession(context.Background(), "agent-x", "Alice", "a@example.com")
	mock := core.NewMockLlmProvider().PushText("hi")

	prompt := assembleSystemPrompt(defaultSystemPrompt, nil, "", true)
	runner := NewTurnRunner(mock, store, prompt, nil, nil, nil, nil, nil, "", "", nil)
	if _, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", "hi", func(map[string]any) {}); err != nil {
		t.Fatalf("run: %v", err)
	}
	if !strings.Contains(systemPromptOf(t, mock), defaultSystemPrompt) {
		t.Error("nil config should keep the default persona")
	}
}

// TestTurnInjectsWorkflowStepAndAdvances asserts the current workflow step is rendered
// into the system prompt and a "yes" judge verdict advances the returned NextStepID.
func TestTurnInjectsWorkflowStepAndAdvances(t *testing.T) {
	store := NewInMemorySessionStore()
	session, _ := store.CreateSession(context.Background(), "agent-x", "Alice", "a@example.com")
	wf := sampleWorkflow()
	// First call = the turn reply; second call = the judge verdict.
	mock := core.NewMockLlmProvider().PushText("Nice to meet you, Alice.").PushText(`{"verdict":"yes"}`)

	prompt := assembleSystemPrompt(defaultSystemPrompt, &AgentConfig{Workflow: wf}, "", true)
	runner := NewTurnRunner(mock, store, prompt, nil, nil, nil, nil, wf, "", "", nil)
	result, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", "hi, I'm Alice", func(map[string]any) {})
	if err != nil {
		t.Fatalf("run: %v", err)
	}

	if !strings.Contains(systemPromptOf(t, mock), "CURRENT STEP (1/3): greet") {
		t.Errorf("turn 1 should render the greet step:\n%s", systemPromptOf(t, mock))
	}
	if result.NextStepID != "qualify" {
		t.Errorf("after a yes verdict, NextStepID = %q, want qualify", result.NextStepID)
	}
}

// TestTurnWorkflowStaysOnStepWhenNotMet asserts a non-yes verdict keeps the pointer put.
func TestTurnWorkflowStaysOnStepWhenNotMet(t *testing.T) {
	store := NewInMemorySessionStore()
	session, _ := store.CreateSession(context.Background(), "agent-x", "Alice", "a@example.com")
	wf := sampleWorkflow()
	mock := core.NewMockLlmProvider().PushText("How can I help?").PushText(`{"verdict":"no"}`)

	runner := NewTurnRunner(mock, store, "", nil, nil, nil, nil, wf, "", "", nil)
	result, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", "hi", func(map[string]any) {})
	if err != nil {
		t.Fatalf("run: %v", err)
	}
	if result.NextStepID != "greet" {
		t.Errorf("a 'no' verdict should stay on greet, got %q", result.NextStepID)
	}
}

// TestTurnWorkflowJudgeFailureDoesNotFailTurn asserts that when the judge call errors (no
// verdict queued), the turn still succeeds and stays on the current step.
func TestTurnWorkflowJudgeFailureDoesNotFailTurn(t *testing.T) {
	store := NewInMemorySessionStore()
	session, _ := store.CreateSession(context.Background(), "agent-x", "Alice", "a@example.com")
	wf := sampleWorkflow()
	// Only the turn reply is queued; the judge call gets an empty-queue error.
	mock := core.NewMockLlmProvider().PushText("Hi there.")

	runner := NewTurnRunner(mock, store, "", nil, nil, nil, nil, wf, "", "", nil)
	result, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", "hi", func(map[string]any) {})
	if err != nil {
		t.Fatalf("a judge failure must not fail the turn: %v", err)
	}
	if result.Reply != "Hi there." {
		t.Errorf("reply = %q, want the turn reply", result.Reply)
	}
	if result.NextStepID != "greet" {
		t.Errorf("judge failure should stay on greet, got %q", result.NextStepID)
	}
}

// TestPerAgentIsolation asserts two agents with different configs (served from one
// resolver) get different system prompts on their own turns — config never bleeds across
// agents.
func TestPerAgentIsolation(t *testing.T) {
	store := NewInMemorySessionStore()
	resolver := NewStaticAgentConfigResolver(map[string]*AgentConfig{
		"agent-a": {Instructions: "You are Ada."},
		"agent-b": {Instructions: "You are Ben."},
	})

	run := func(agentID string) string {
		session, _ := store.CreateSession(context.Background(), agentID, "U", "u@example.com")
		cfg, _ := resolver.Resolve(context.Background(), agentID)
		prompt := assembleSystemPrompt(defaultSystemPrompt, cfg, session.CurrentStepID, true)
		mock := core.NewMockLlmProvider().PushText("ok")
		runner := NewTurnRunner(mock, store, prompt, nil, nil, nil, nil, nil, "", "", nil)
		if _, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r", "hi", func(map[string]any) {}); err != nil {
			t.Fatalf("run %s: %v", agentID, err)
		}
		return systemPromptOf(t, mock)
	}

	if a := run("agent-a"); !strings.Contains(a, "You are Ada.") || strings.Contains(a, "You are Ben.") {
		t.Errorf("agent-a prompt leaked or wrong:\n%s", a)
	}
	if b := run("agent-b"); !strings.Contains(b, "You are Ben.") || strings.Contains(b, "You are Ada.") {
		t.Errorf("agent-b prompt leaked or wrong:\n%s", b)
	}
}
