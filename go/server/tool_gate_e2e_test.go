package server

import (
	"context"
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// e2eAgentID matches the agentId createSession sends, so the resolver keys on it.
const e2eAgentID = "11111111-1111-1111-1111-111111111111"

// gateScenario drives one scripted tool-call turn through the REAL dispatcher/WS path with
// per-agent config, returning the tool-result text the model saw and whether the tool's Fn
// actually ran. The mock scripts: the tool call, then a wrap-up reply.
func gateScenario(t *testing.T, cfg *AgentConfig, authRequiring []string, auth SessionAuthenticator, capturedArgs *map[string]any) (toolResult string, ran bool) {
	t.Helper()
	const toolName = "lookup_orders"

	mock := core.NewMockLlmProvider()
	mock.PushToolCall("call-1", toolName, `{"q":"my orders"}`)
	mock.PushText("Here's what I found.")

	tool := core.FuncTool{
		ToolName: toolName,
		Desc:     "Look up the user's orders.",
		Params:   map[string]any{"type": "object", "properties": map[string]any{"q": map[string]any{"type": "string"}}},
		Fn: func(_ context.Context, args map[string]any) (string, error) {
			ran = true
			if capturedArgs != nil {
				*capturedArgs = args
			}
			return "ORDERS: #42, #43", nil
		},
	}

	opts := []LocalOption{
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(mock),
		WithLocalServerOption(WithTools([]core.Tool{tool})),
		WithLocalServerOption(WithAgentConfigResolver(NewStaticAgentConfigResolver(map[string]*AgentConfig{e2eAgentID: cfg}))),
	}
	if len(authRequiring) > 0 {
		opts = append(opts, WithLocalServerOption(WithAuthRequiringTools(authRequiring...)))
	}
	if auth != nil {
		opts = append(opts, WithLocalServerOption(WithSessionAuthenticator(auth)))
	}

	ls, err := SpawnLocal(opts...)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer ls.Shutdown()

	transport := connectTransport(t, ls)
	defer transport.Close()
	sessionID := createSession(t, transport)

	sendFrame(t, transport, map[string]any{
		"action":    "send_message",
		"requestId": "r-msg",
		"sessionId": sessionID,
		"message":   "show me my orders",
	})

	// Scan to the terminal event, capturing the tool-result text the model was fed.
	for {
		ev := nextEv(t, transport)
		typ, _ := ev["type"].(string)
		if typ == "stream_chunk" {
			if res, ok := dot(t, ev, "data.state.rawResponse.toolResult.result"); ok {
				if s, _ := res.(string); s != "" {
					toolResult = s
				}
			}
		}
		if typ == "eventual_response" {
			break
		}
	}
	return toolResult, ran
}

// TestToolGateE2E drives the auth gate end-to-end through the real dispatcher for each
// row of the reference matrix (admin/end_user × public/internal × authed).
func TestToolGateE2E(t *testing.T) {
	const toolName = "lookup_orders"

	t.Run("admin on public agent blocked", func(t *testing.T) {
		cfg := &AgentConfig{Visibility: "public", EnabledTools: []EnabledTool{{ToolID: toolName, Enabled: true, AuthLevel: "admin"}}}
		res, ran := gateScenario(t, cfg, []string{toolName}, nil, nil)
		if ran {
			t.Error("admin tool must NOT execute on a public agent")
		}
		if !strings.Contains(res, "requires admin authentication") {
			t.Errorf("tool result = %q, want admin-block message", res)
		}
	})

	t.Run("end_user unauthenticated blocked", func(t *testing.T) {
		cfg := &AgentConfig{Visibility: "public", EnabledTools: []EnabledTool{{ToolID: toolName, Enabled: true, AuthLevel: "end_user"}}}
		res, ran := gateScenario(t, cfg, []string{toolName}, nil, nil)
		if ran {
			t.Error("end_user tool must NOT execute when unauthenticated")
		}
		if !strings.Contains(res, "verify your identity") {
			t.Errorf("tool result = %q, want identity-verification message", res)
		}
	})

	t.Run("end_user authenticated executes", func(t *testing.T) {
		cfg := &AgentConfig{Visibility: "public", EnabledTools: []EnabledTool{{ToolID: toolName, Enabled: true, AuthLevel: "end_user"}}}
		res, ran := gateScenario(t, cfg, []string{toolName}, stubAuth(true), nil)
		if !ran {
			t.Error("end_user tool must execute once authenticated")
		}
		if !strings.Contains(res, "ORDERS:") {
			t.Errorf("tool result = %q, want the real tool output", res)
		}
	})

	t.Run("internal agent auto-satisfied", func(t *testing.T) {
		cfg := &AgentConfig{Visibility: "internal", EnabledTools: []EnabledTool{{ToolID: toolName, Enabled: true, AuthLevel: "admin"}}}
		res, ran := gateScenario(t, cfg, []string{toolName}, nil, nil)
		if !ran {
			t.Error("admin tool must execute on an internal agent (auto-satisfied)")
		}
		if !strings.Contains(res, "ORDERS:") {
			t.Errorf("tool result = %q, want the real tool output", res)
		}
	})

	t.Run("per-tool config reaches the tool", func(t *testing.T) {
		var gotArgs map[string]any
		cfg := &AgentConfig{EnabledTools: []EnabledTool{{ToolID: toolName, Enabled: true, AuthLevel: "none", Config: map[string]any{"region": "us"}}}}
		_, ran := gateScenario(t, cfg, nil, nil, &gotArgs)
		if !ran {
			t.Fatal("tool with authLevel none must execute")
		}
		cfgArg, ok := gotArgs[toolConfigArgKey].(map[string]any)
		if !ok || cfgArg["region"] != "us" {
			t.Errorf("per-tool config did not reach the tool: %v", gotArgs)
		}
	})
}

// TestWorkflowAdvancesAcrossTurnsE2E drives two send_message turns over the real WS/
// dispatcher path against a shared store, asserting the judge-advanced CurrentStepID is
// persisted on the session and progresses turn to turn (greet → qualify → book).
func TestWorkflowAdvancesAcrossTurnsE2E(t *testing.T) {
	store := NewInMemorySessionStore()
	cfg := &AgentConfig{Workflow: sampleWorkflow()}

	mock := core.NewMockLlmProvider()
	// Turn 1: reply, then judge "yes" → advance greet→qualify.
	mock.PushText("Hi, who am I speaking with?").PushText(`{"verdict":"yes"}`)
	// Turn 2: reply, then judge "yes" → advance qualify→book.
	mock.PushText("Got it — what are you looking to do?").PushText(`{"verdict":"yes"}`)

	ls, err := SpawnLocal(
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(mock),
		WithLocalServerOption(WithSessionStore(store)),
		WithLocalServerOption(WithAgentConfigResolver(NewStaticAgentConfigResolver(map[string]*AgentConfig{e2eAgentID: cfg}))),
	)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer ls.Shutdown()

	transport := connectTransport(t, ls)
	defer transport.Close()
	sessionID := createSession(t, transport)

	sendTurn := func(reqID, msg string) {
		sendFrame(t, transport, map[string]any{"action": "send_message", "requestId": reqID, "sessionId": sessionID, "message": msg})
		for {
			if typ, _ := nextEv(t, transport)["type"].(string); typ == "eventual_response" {
				return
			}
		}
	}
	stepOf := func() string {
		s, _ := store.GetSession(context.Background(), sessionID)
		if s == nil {
			t.Fatal("session vanished")
		}
		return s.CurrentStepID
	}

	sendTurn("r-1", "hello")
	if got := stepOf(); got != "qualify" {
		t.Fatalf("after turn 1 (yes), current step = %q, want qualify", got)
	}
	sendTurn("r-2", "I run a bakery")
	if got := stepOf(); got != "book" {
		t.Fatalf("after turn 2 (yes), current step = %q, want book", got)
	}
}
