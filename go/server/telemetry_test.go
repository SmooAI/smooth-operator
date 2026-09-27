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
//  2. A child `gen_ai.tool` span carrying gen_ai.tool.name and the argument KEY NAMES
//     (gen_ai.tool.argument_keys) — never the argument values (SMOODEV-3364).
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
	// A model the engine's local DefaultPricing table knows, so the mock turn is priced
	// (the table carries only claude-haiku-4-5 / claude-sonnet-4-5; the server default
	// gpt-6-luna is unpriced locally and relies on the gateway's cost header). Since
	// SMOODEV-3342 the runner's model is what the engine actually requests, not span-only.
	runner.model = "claude-haiku-4-5"
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
	assertAttr(t, chat.Attributes, GenAIRequestModel, "claude-haiku-4-5")
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
	// SMOODEV-3364: argument values are customer PII, so the span carries the shape of
	// the call and never its content.
	assertAttr(t, tool.Attributes, GenAIToolArgumentKeys, "api_key,query")
	if args, ok := attr(tool.Attributes, GenAIToolArguments); ok {
		t.Errorf("the tool span must not carry %s; got: %q", GenAIToolArguments, args)
	}
	for _, kv := range tool.Attributes {
		v := kv.Value.Emit()
		if strings.Contains(v, "return policy refund window") || strings.Contains(v, "sk-live-123") {
			t.Errorf("span attribute %s leaked an argument value: %q", kv.Key, v)
		}
	}

	// The tool span is a CHILD of the turn span (mirrors the Rust `parent: &turn_span`).
	if tool.Parent.SpanID() != chat.SpanContext.SpanID() {
		t.Errorf("gen_ai.tool span should be a child of gen_ai.chat; parent=%s chat=%s",
			tool.Parent.SpanID(), chat.SpanContext.SpanID())
	}

	// Being a child is NOT enough. The OTLP ingest builds a span's attributes from the
	// resource attrs plus THAT span's own, with no parent inheritance, so the tool span
	// repeats the identifiers itself — and without gen_ai.system it fails the ingest's
	// LLM-event gate outright and is discarded, which is what happened to Rust's tool
	// spans for their entire existence (zero rows with operation_name='tool', all time).
	assertAttr(t, tool.Attributes, GenAISystem, SystemName)
	assertAttr(t, tool.Attributes, GenAIOperationName, OperationTool)
	assertAttr(t, tool.Attributes, GenAIConversationID, session.ConversationID)
	assertAttr(t, tool.Attributes, SmooaiOrgID, "org-telemetry")

	// Must be exactly "chat"/"tool" — the ingest takes the attribute verbatim when
	// present and its queries filter on operation_name = 'tool'.
	assertAttr(t, chat.Attributes, GenAIOperationName, OperationChat)

	// Cost: exactly one of the two is ever set. The mock turn IS priced (local
	// DefaultPricing knows claude-haiku-4-5), so the cost lands and the marker must not —
	// a zero must never be exported as a real cost, and a real cost must never carry
	// an "unavailable" marker beside it.
	cost, hasCost := attr(chat.Attributes, GenAIUsageCostUSD)
	if !hasCost {
		t.Errorf("a priced turn must record %s; got attrs %+v", GenAIUsageCostUSD, chat.Attributes)
	} else if cost == "0" || cost == "0.000000" {
		t.Errorf("%s must never be exported as zero — that means unpriced, not free", GenAIUsageCostUSD)
	}
	if _, ok := attr(chat.Attributes, CostUnavailable); ok {
		t.Errorf("%s must not be set alongside a real cost", CostUnavailable)
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

// TestToolArgumentKeysNeverCarryValues is the Go sibling of the Rust
// tool_argument_keys_never_carry_values: the shared vectors, asserted verbatim.
func TestToolArgumentKeysNeverCarryValues(t *testing.T) {
	vectors := []struct{ in, want string }{
		{`{"name":"Jane Customer","email":"jane@example.com","phone":"(317) 555-0142"}`, "email,name,phone"},
		{`{"b":1,"a":{"nested":"jane@example.com"}}`, "a,b"},
		{`{}`, ""},
		{`null`, ""},
		{``, ""},
		{`["jane@example.com"]`, "<array>"},
		{`"jane@example.com"`, "<scalar>"},
		{`{"email":"jane@exa`, "<unparsed>"},
	}
	for _, v := range vectors {
		got := toolArgumentKeys(v.in)
		if got != v.want {
			t.Errorf("toolArgumentKeys(%q) = %q, want %q", v.in, got, v.want)
		}
		for _, raw := range []string{"jane@example.com", "Jane Customer", "555-0142"} {
			if strings.Contains(got, raw) {
				t.Errorf("toolArgumentKeys(%q) leaked %q: %q", v.in, raw, got)
			}
		}
	}
}
