package server

import (
	"context"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// SMOODEV-3342 parity with the Rust reference (smooth-operator-server config.rs
// `defaults_apply_when_env_absent` / `judge_model_defaults_to_groq_and_env_overrides`):
// the main turn defaults to gpt-6-luna and the workflow judge has its OWN default,
// groq-gpt-oss-120b — never the engine's built-in fallback model.

func TestDefaultModelIsGpt6Luna(t *testing.T) {
	if DefaultModel != "gpt-6-luna" {
		t.Fatalf("DefaultModel = %q, want gpt-6-luna", DefaultModel)
	}
}

func TestDefaultJudgeModelIsGroqAndDistinctFromTurnModel(t *testing.T) {
	if DefaultJudgeModel != "groq-gpt-oss-120b" {
		t.Fatalf("DefaultJudgeModel = %q, want groq-gpt-oss-120b", DefaultJudgeModel)
	}
	if DefaultJudgeModel == DefaultModel {
		t.Fatalf("the judge must have its own default, not the main-turn model")
	}
}

// A turn with no explicit model must REQUEST DefaultModel from the gateway — not leave
// the model empty so the engine substitutes its own built-in default.
func TestTurnRequestsDefaultModelWhenUnset(t *testing.T) {
	t.Setenv("SMOOTH_AGENT_PREAMBLE_MODEL", "")
	client := &scriptedClient{handle: func(_ context.Context, _ core.ChatRequest) (core.ChatResponse, error) {
		return core.ChatResponse{Content: "hello"}, nil
	}}

	runTurnWith(t, client, "hi")

	client.mu.Lock()
	defer client.mu.Unlock()
	if len(client.models) == 0 {
		t.Fatalf("expected at least one model call")
	}
	for _, m := range client.models {
		if m != DefaultModel {
			t.Fatalf("turn requested model %q, want %q", m, DefaultModel)
		}
	}
}

func TestUnsavedOrgSettingsReportDefaultModel(t *testing.T) {
	if got := defaultSettings("org-1").Model; got != DefaultModel {
		t.Fatalf("defaultSettings model = %q, want %q", got, DefaultModel)
	}
}

// judgeCaptureClient records the judge request so a test can assert its model + cap.
type judgeCaptureClient struct{ req core.ChatRequest }

func (c *judgeCaptureClient) Chat(_ context.Context, req core.ChatRequest) (core.ChatResponse, error) {
	c.req = req
	return core.ChatResponse{Content: `{"verdict":"yes"}`}, nil
}

// Parity with the Rust JUDGE_MAX_TOKENS (SMOODEV-3342): the default judge is a reasoning
// model, so the cap must leave room for reasoning — a small cap returns an empty reply.
func TestJudgeRequestsDefaultJudgeModelWithReasoningHeadroom(t *testing.T) {
	client := &judgeCaptureClient{}
	if v := judgeWorkflowStep(t.Context(), client, "", sampleWorkflow(), "greet", "hi", "Hello!"); v != VerdictYes {
		t.Fatalf("verdict = %s, want yes", v)
	}
	if client.req.Model != DefaultJudgeModel {
		t.Errorf("judge model = %q, want %q", client.req.Model, DefaultJudgeModel)
	}
	if JudgeMaxTokens != 512 || client.req.MaxTokens != 512 {
		t.Errorf("judge max_tokens = %d (const %d), want 512", client.req.MaxTokens, JudgeMaxTokens)
	}
}
