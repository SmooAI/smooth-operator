package server

import (
	"context"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

func ceilPtr(v int) *int { return &v }

// TestClampMaxTokens covers the clamp: clamp-down, passthrough, nil, and the
// never-0 guarantee (EPIC th-1cc9fa).
func TestClampMaxTokens(t *testing.T) {
	cases := []struct {
		name       string
		configured int
		ceiling    *int
		want       int
	}{
		{"nil ceiling passes through", 8192, nil, 8192},
		{"ceiling below budget clamps down", 8192, ceilPtr(4096), 4096},
		{"ceiling above budget passes through", 8192, ceilPtr(16384), 8192},
		{"ceiling equal to budget passes through", 8192, ceilPtr(8192), 8192},
		{"zero ceiling treated as unknown", 8192, ceilPtr(0), 8192},
		{"negative ceiling treated as unknown", 8192, ceilPtr(-5), 8192},
		{"never clamps a positive budget to 0", 8192, ceilPtr(1), 1},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := clampMaxTokens(tc.configured, tc.ceiling)
			if got != tc.want {
				t.Fatalf("clampMaxTokens(%d, %v) = %d, want %d", tc.configured, tc.ceiling, got, tc.want)
			}
			if got == 0 && tc.configured > 0 {
				t.Fatalf("clampMaxTokens clamped a positive budget to 0")
			}
		})
	}
}

// TestModelOutputCeiling covers extracting max_output_tokens from a /model/info body:
// present, missing model, missing/zero ceiling, and malformed JSON all resolve
// correctly (unknown ⇒ nil ⇒ unclamped).
func TestModelOutputCeiling(t *testing.T) {
	payload := []byte(`{
		"data": [
			{"model_name": "groq-compound", "model_info": {"max_output_tokens": 8192}},
			{"model_name": "claude-sonnet-4-5", "model_info": {"max_output_tokens": 65536}},
			{"model_name": "no-ceiling", "model_info": {"input_cost_per_token": 0.000001}},
			{"model_name": "zero-ceiling", "model_info": {"max_output_tokens": 0}}
		]
	}`)
	cases := []struct {
		name  string
		body  []byte
		model string
		want  *int
	}{
		{"extracts the model's ceiling", payload, "groq-compound", ceilPtr(8192)},
		{"extracts a different model's ceiling", payload, "claude-sonnet-4-5", ceilPtr(65536)},
		{"absent model → nil", payload, "gpt-9", nil},
		{"model present but no ceiling → nil", payload, "no-ceiling", nil},
		{"zero ceiling → nil (unknown)", payload, "zero-ceiling", nil},
		{"malformed JSON → nil", []byte("not json"), "groq-compound", nil},
		{"empty body → nil", []byte(""), "groq-compound", nil},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := modelOutputCeiling(tc.body, tc.model)
			switch {
			case tc.want == nil && got != nil:
				t.Fatalf("got %d, want nil", *got)
			case tc.want != nil && got == nil:
				t.Fatalf("got nil, want %d", *tc.want)
			case tc.want != nil && *got != *tc.want:
				t.Fatalf("got %d, want %d", *got, *tc.want)
			}
		})
	}
}

// TestRunnerAppliesRaisedDefaultsAndCeiling proves the wiring end-to-end: a turn's
// request carries the raised default max_tokens (8192) clamped to the configured
// model ceiling, not the old starvation-prone 512.
func TestRunnerAppliesRaisedDefaultsAndCeiling(t *testing.T) {
	t.Run("clamped to ceiling below default", func(t *testing.T) {
		mock := core.NewMockLlmProvider().PushText("hi")
		store := NewInMemorySessionStore()
		session, err := store.CreateSession(context.Background(), "agent-1", "A", "a@example.com", ConversationScope{Unscoped: true})
		if err != nil {
			t.Fatalf("create session: %v", err)
		}
		runner := NewTurnRunner(mock, store, "", nil, nil, nil, nil, nil, "", "", ceilPtr(4096))
		if _, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", "hello", func(map[string]any) {}); err != nil {
			t.Fatalf("run: %v", err)
		}
		call, ok := mock.LastCall()
		if !ok {
			t.Fatal("no model call recorded")
		}
		if call.MaxTokens != 4096 {
			t.Fatalf("request MaxTokens = %d, want clamped 4096", call.MaxTokens)
		}
	})

	t.Run("raised default when no ceiling", func(t *testing.T) {
		mock := core.NewMockLlmProvider().PushText("hi")
		store := NewInMemorySessionStore()
		session, _ := store.CreateSession(context.Background(), "agent-1", "A", "a@example.com", ConversationScope{Unscoped: true})
		runner := NewTurnRunner(mock, store, "", nil, nil, nil, nil, nil, "", "", nil)
		if _, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", "hello", func(map[string]any) {}); err != nil {
			t.Fatalf("run: %v", err)
		}
		call, _ := mock.LastCall()
		if call.MaxTokens != DefaultMaxTokens {
			t.Fatalf("request MaxTokens = %d, want raised default %d", call.MaxTokens, DefaultMaxTokens)
		}
		if DefaultMaxTokens != 8192 || DefaultMaxIterations != 20 {
			t.Fatalf("defaults = %d/%d, want 8192/20", DefaultMaxTokens, DefaultMaxIterations)
		}
	})
}
