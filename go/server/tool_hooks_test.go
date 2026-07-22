package server

import (
	"context"
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// recordingHook records the tool names it sees on Pre/PostCall.
type recordingHook struct {
	pre  []string
	post []string
}

func (h *recordingHook) PreCall(_ context.Context, call core.ToolCall) error {
	h.pre = append(h.pre, call.Name)
	return nil
}

func (h *recordingHook) PostCall(_ context.Context, call core.ToolCall, _ *core.ToolResult) error {
	h.post = append(h.post, call.Name)
	return nil
}

// secretRedactHook scrubs "secret" from a tool result (the redaction seam).
type secretRedactHook struct{}

func (secretRedactHook) PreCall(_ context.Context, _ core.ToolCall) error { return nil }
func (secretRedactHook) PostCall(_ context.Context, _ core.ToolCall, result *core.ToolResult) error {
	result.Content = strings.ReplaceAll(result.Content, "secret", "[REDACTED]")
	return nil
}

func leakTool(text string) core.Tool {
	return core.FuncTool{
		ToolName: "lookup",
		Desc:     "Looks something up.",
		Params:   map[string]any{"type": "object"},
		Fn:       func(_ context.Context, _ map[string]any) (string, error) { return text, nil },
	}
}

// runHookedTurn drives one turn through a TurnRunner wired the way the dispatcher
// wires it (tools + hooks), scripting the engine to call `lookup` then reply.
func runHookedTurn(t *testing.T, tools []core.Tool, hooks []core.ToolHook) *core.MockLlmProvider {
	t.Helper()
	store := NewInMemorySessionStore()
	session, err := store.CreateSession(context.Background(), "agent-1", "Alice", "alice@example.com")
	if err != nil {
		t.Fatalf("create session: %v", err)
	}
	mock := core.NewMockLlmProvider().
		PushToolCall("c1", "lookup", `{}`).
		PushText("done")
	runner := NewTurnRunner(mock, store, "", nil, tools, nil, nil, nil, "", "", nil)
	runner.hooks = hooks
	if _, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", "look it up", func(map[string]any) {}); err != nil {
		t.Fatalf("run turn: %v", err)
	}
	return mock
}

// TestWithToolHooksInstallsHooks asserts the WithToolHooks option lands the hooks on
// the Server so they ride the tools seam into a turn.
func TestWithToolHooksInstallsHooks(t *testing.T) {
	h := &recordingHook{}
	srv := &Server{}
	WithToolHooks(h)(srv)
	if len(srv.hooks) != 1 {
		t.Fatalf("WithToolHooks should install one hook, got %d", len(srv.hooks))
	}
}

// TestHookFiresThroughTurnRunner asserts a hook installed via the dispatcher seam
// fires Pre+Post around a dispatched tool.
func TestHookFiresThroughTurnRunner(t *testing.T) {
	h := &recordingHook{}
	runHookedTurn(t, []core.Tool{leakTool("ok")}, []core.ToolHook{h})
	if len(h.pre) != 1 || h.pre[0] != "lookup" {
		t.Fatalf("PreCall should fire for lookup; got %v", h.pre)
	}
	if len(h.post) != 1 || h.post[0] != "lookup" {
		t.Fatalf("PostCall should fire for lookup; got %v", h.post)
	}
}

// TestPostCallRedactionReachesModel asserts a PostCall redaction rewrites the tool
// result the model then sees on its follow-up call.
func TestPostCallRedactionReachesModel(t *testing.T) {
	mock := runHookedTurn(t, []core.Tool{leakTool("the secret token is abc")}, []core.ToolHook{secretRedactHook{}})
	// The engine's SECOND model call carries the tool-result message; it must be redacted.
	calls := mock.Calls()
	if len(calls) < 2 {
		t.Fatalf("expected a follow-up model call after the tool ran, got %d", len(calls))
	}
	var toolMsg string
	for _, m := range calls[1].Messages {
		if m.Role == "tool" {
			toolMsg = m.Content
		}
	}
	if strings.Contains(toolMsg, "secret") || !strings.Contains(toolMsg, "[REDACTED]") {
		t.Fatalf("redaction must reach the model; got %q", toolMsg)
	}
}
