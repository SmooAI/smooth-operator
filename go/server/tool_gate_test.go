package server

import (
	"context"
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// stubAuth is a fixed SessionAuthenticator verdict for tests.
type stubAuth bool

func (s stubAuth) IsAuthenticated(context.Context, string) (bool, error) { return bool(s), nil }

// recTool is a core.FuncTool that records whether it ran and the args it received.
func recTool(name string, ran *bool, gotArgs *map[string]any) core.Tool {
	return core.FuncTool{
		ToolName: name,
		Desc:     "test",
		Params:   map[string]any{"type": "object"},
		Fn: func(_ context.Context, args map[string]any) (string, error) {
			*ran = true
			*gotArgs = args
			return "REAL:" + name, nil
		},
	}
}

func TestGatedToolExecute(t *testing.T) {
	tests := []struct {
		name         string
		authLevel    string
		supportsAuth bool
		visibility   string
		auth         SessionAuthenticator
		wantRan      bool
		wantContains string // substring of the returned message (blocked path)
	}{
		{name: "not auth-requiring tool executes even at admin", authLevel: "admin", supportsAuth: false, visibility: "public", wantRan: true, wantContains: "REAL:"},
		{name: "authLevel none executes", authLevel: "none", supportsAuth: true, visibility: "public", wantRan: true, wantContains: "REAL:"},
		{name: "empty authLevel executes", authLevel: "", supportsAuth: true, visibility: "public", wantRan: true, wantContains: "REAL:"},
		{name: "admin on public blocked", authLevel: "admin", supportsAuth: true, visibility: "public", wantRan: false, wantContains: "requires admin authentication and is not available on public-facing agents"},
		{name: "admin on internal auto-satisfied", authLevel: "admin", supportsAuth: true, visibility: "internal", wantRan: true, wantContains: "REAL:"},
		{name: "end_user public no authenticator blocked", authLevel: "end_user", supportsAuth: true, visibility: "public", auth: nil, wantRan: false, wantContains: "verify your identity"},
		{name: "end_user public unauthenticated blocked", authLevel: "end_user", supportsAuth: true, visibility: "public", auth: stubAuth(false), wantRan: false, wantContains: "verify your identity"},
		{name: "end_user public authenticated executes", authLevel: "end_user", supportsAuth: true, visibility: "public", auth: stubAuth(true), wantRan: true, wantContains: "REAL:"},
		{name: "end_user internal auto-satisfied", authLevel: "end_user", supportsAuth: true, visibility: "internal", wantRan: true, wantContains: "REAL:"},
		{name: "empty visibility treated as public (admin blocked)", authLevel: "admin", supportsAuth: true, visibility: "", wantRan: false, wantContains: "requires admin authentication"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			ran := false
			var gotArgs map[string]any
			g := gatedTool{
				Tool:           recTool("t", &ran, &gotArgs),
				authLevel:      tt.authLevel,
				supportsAuth:   tt.supportsAuth,
				visibility:     tt.visibility,
				authenticator:  tt.auth,
				conversationID: "conv-1",
			}
			out, err := g.Execute(context.Background(), map[string]any{})
			if err != nil {
				t.Fatalf("execute: %v", err)
			}
			if ran != tt.wantRan {
				t.Errorf("ran = %v, want %v", ran, tt.wantRan)
			}
			if !strings.Contains(out, tt.wantContains) {
				t.Errorf("output %q missing %q", out, tt.wantContains)
			}
		})
	}
}

func TestGatedToolDeliversConfig(t *testing.T) {
	ran := false
	var gotArgs map[string]any
	g := gatedTool{
		Tool:   recTool("t", &ran, &gotArgs),
		config: map[string]any{"apiBase": "https://x", "limit": 5},
	}
	if _, err := g.Execute(context.Background(), map[string]any{"q": "hi"}); err != nil {
		t.Fatalf("execute: %v", err)
	}
	cfg, ok := gotArgs[toolConfigArgKey].(map[string]any)
	if !ok {
		t.Fatalf("tool did not receive config under %q: %v", toolConfigArgKey, gotArgs)
	}
	if cfg["apiBase"] != "https://x" {
		t.Errorf("config not delivered: %v", cfg)
	}
	// The model's own args are preserved alongside the namespaced config.
	if gotArgs["q"] != "hi" {
		t.Errorf("model args should be preserved: %v", gotArgs)
	}
}

func TestGateToolsWrapsOnlyWhenNeeded(t *testing.T) {
	tools := []core.Tool{stubTool("plain"), stubTool("gated"), stubTool("configured")}
	cfg := &AgentConfig{
		Visibility: "public",
		EnabledTools: []EnabledTool{
			{ToolID: "gated", Enabled: true, AuthLevel: "admin"},
			{ToolID: "configured", Enabled: true, AuthLevel: "none", Config: map[string]any{"k": "v"}},
			// "plain" has no entry.
		},
	}
	got := gateTools(tools, cfg, map[string]bool{"gated": true}, nil, "conv-1", nil)

	// plain: no entry → passthrough (not wrapped).
	if _, wrapped := got[0].(gatedTool); wrapped {
		t.Error("plain tool should not be wrapped")
	}
	// gated: auth-requiring + admin → wrapped.
	if _, wrapped := got[1].(gatedTool); !wrapped {
		t.Error("gated tool should be wrapped")
	}
	// configured: has config → wrapped (for delivery) even at authLevel none.
	if _, wrapped := got[2].(gatedTool); !wrapped {
		t.Error("configured tool should be wrapped for config delivery")
	}

	// nil config → tools unchanged.
	if same := gateTools(tools, nil, nil, nil, "c", nil); len(same) != 3 {
		t.Fatalf("nil config should return tools unchanged")
	}
}

func TestParseVisibility(t *testing.T) {
	if cfg := ParseAgentConfig([]byte(`{"visibility":"internal"}`)); cfg == nil || cfg.Visibility != "internal" {
		t.Errorf("internal visibility not parsed: %+v", cfg)
	}
	// public is the default → not a "populated" signal on its own.
	if cfg := ParseAgentConfig([]byte(`{"visibility":"public"}`)); cfg != nil {
		t.Errorf("public-only config should be nil (default), got %+v", cfg)
	}
}
