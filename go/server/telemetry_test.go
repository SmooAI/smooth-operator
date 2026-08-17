package server

import (
	"context"
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/sdk/trace/tracetest"
)

// attr looks up a span attribute value as a string, "" when absent.
func attr(kvs []attribute.KeyValue, key string) (string, bool) {
	for _, kv := range kvs {
		if string(kv.Key) == key {
			return kv.Value.Emit(), true
		}
	}
	return "", false
}

// TestStreamingTurnEmitsGenAISpans is the Go sibling of the Rust server's
// tests/telemetry.rs: it drives a real streaming turn (a knowledge_search tool call then
// a final answer) through the TurnRunner and asserts — via an in-memory span exporter, no
// live OTLP collector — that the turn emits:
//
//  1. A `gen_ai.chat` turn span carrying gen_ai.system, gen_ai.request.model,
//     gen_ai.conversation.id, gen_ai.agent.name, and smooai.org_id.
//  2. A child `gen_ai.tool` span carrying gen_ai.tool.name and the REDACTED
//     gen_ai.tool.call.arguments the model passed.
func TestStreamingTurnEmitsGenAISpans(t *testing.T) {
	// Install an in-memory exporter as the global provider for the turn, then restore.
	exporter := tracetest.NewInMemoryExporter()
	tp := sdktrace.NewTracerProvider(sdktrace.WithSyncer(exporter))
	prev := otel.GetTracerProvider()
	otel.SetTracerProvider(tp)
	t.Cleanup(func() { otel.SetTracerProvider(prev) })

	store := NewInMemorySessionStore()
	session, err := store.CreateSession(context.Background(), "agent-1", "Alice", "alice@example.com", ConversationScope{Unscoped: true})
	if err != nil {
		t.Fatalf("create session: %v", err)
	}

	// Script the mock for the STREAMING path: turn 1 calls knowledge_search with args
	// (including a secret-named field the span must scrub), turn 2 answers.
	mock := core.NewMockLlmProvider().
		PushToolCall("call_kb_1", "knowledge_search", `{"query":"return policy refund window","api_key":"sk-live-123"}`).
		PushText("Items are accepted within 30 days for a full refund.")

	kbTool := core.FuncTool{
		ToolName: "knowledge_search",
		Desc:     "Search the knowledge base.",
		Params:   map[string]any{"type": "object"},
		Fn: func(context.Context, map[string]any) (string, error) {
			return "Returns are accepted within 30 days for a full refund.", nil
		},
	}

	runner := NewTurnRunner(mock, store, "", nil, []core.Tool{kbTool}, nil, nil, nil, "", "", nil)
	runner.model = "openai/gpt-4o"
	runner.orgID = "org-telemetry"

	if _, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "req-otel", "what is the return policy?", func(map[string]any) {}); err != nil {
		t.Fatalf("run turn: %v", err)
	}

	spans := exporter.GetSpans()

	// (1) The turn span carries system, model, conversation, agent, and org.
	var chat *tracetest.SpanStub
	for i := range spans {
		if spans[i].Name == SpanChat {
			chat = &spans[i]
			break
		}
	}
	if chat == nil {
		t.Fatalf("expected a %q span; got %d spans: %+v", SpanChat, len(spans), spans)
	}
	assertAttr(t, chat.Attributes, GenAISystem, SystemName)
	assertAttr(t, chat.Attributes, GenAIRequestModel, "openai/gpt-4o")
	assertAttr(t, chat.Attributes, GenAIConversationID, session.ConversationID)
	assertAttr(t, chat.Attributes, GenAIAgentName, AgentName)
	assertAttr(t, chat.Attributes, SmooaiOrgID, "org-telemetry")

	// (2) A child tool span with the tool name + redacted arguments.
	var tool *tracetest.SpanStub
	for i := range spans {
		if spans[i].Name == SpanTool {
			tool = &spans[i]
			break
		}
	}
	if tool == nil {
		t.Fatalf("expected a %q span; got %d spans: %+v", SpanTool, len(spans), spans)
	}
	assertAttr(t, tool.Attributes, GenAIToolName, "knowledge_search")
	args, _ := attr(tool.Attributes, GenAIToolArguments)
	if !strings.Contains(args, "return policy refund window") {
		t.Errorf("tool arguments should carry the model's query; got: %q", args)
	}
	if strings.Contains(args, "sk-live-123") {
		t.Errorf("secret-named api_key value must be redacted from the span; got: %q", args)
	}

	// The tool span is a CHILD of the turn span (mirrors the Rust `parent: &turn_span`).
	if tool.Parent.SpanID() != chat.SpanContext.SpanID() {
		t.Errorf("gen_ai.tool span should be a child of gen_ai.chat; parent=%s chat=%s",
			tool.Parent.SpanID(), chat.SpanContext.SpanID())
	}
}

func assertAttr(t *testing.T, kvs []attribute.KeyValue, key, want string) {
	t.Helper()
	got, ok := attr(kvs, key)
	if !ok {
		t.Errorf("span missing attribute %q (want %q)", key, want)
		return
	}
	if got != want {
		t.Errorf("attribute %q = %q, want %q", key, got, want)
	}
}
