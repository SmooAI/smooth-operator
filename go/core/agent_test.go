package core

import (
	"context"
	"strings"
	"testing"
)

// fakeClient returns scripted responses and records the requests it saw.
type fakeClient struct {
	scripted []ChatResponse
	calls    []ChatRequest
}

func (f *fakeClient) Chat(_ context.Context, req ChatRequest) (ChatResponse, error) {
	f.calls = append(f.calls, req)
	resp := f.scripted[0]
	f.scripted = f.scripted[1:]
	return resp, nil
}

func TestKnowledgeRanksByOverlap(t *testing.T) {
	kb := &InMemoryKnowledge{}
	kb.Ingest("The return window is 17 days from delivery.", "returns.md")
	kb.Ingest("Gift wrapping costs 4.99 per item.", "wrapping.md")
	hits := kb.Query("what is the return window?", 1)
	if len(hits) != 1 {
		t.Fatalf("want 1 hit, got %d", len(hits))
	}
	if !strings.Contains(hits[0].Content, "17 days") {
		t.Fatalf("top hit should be the returns doc, got %q", hits[0].Content)
	}
}

func TestTextReplyStopsAfterOneCall(t *testing.T) {
	client := &fakeClient{scripted: []ChatResponse{{Content: "the answer is 42"}}}
	agent := NewSmoothAgent(client, AgentOptions{Instructions: "be helpful"})
	res, err := agent.Run(context.Background(), "what is the answer?", nil)
	if err != nil {
		t.Fatal(err)
	}
	if res.Text != "the answer is 42" || res.Iterations != 1 || res.ToolCalls != 0 {
		t.Fatalf("unexpected result: %+v", res)
	}
}

func TestToolCallThenFinish(t *testing.T) {
	echo := FuncTool{
		ToolName: "echo",
		Desc:     "Echoes input back",
		Params:   map[string]any{"type": "object", "properties": map[string]any{"text": map[string]any{"type": "string"}}, "required": []string{"text"}},
		Fn: func(_ context.Context, args map[string]any) (string, error) {
			s, _ := args["text"].(string)
			return s, nil
		},
	}
	client := &fakeClient{scripted: []ChatResponse{
		{ToolCalls: []ToolCall{{ID: "call-1", Name: "echo", Arguments: `{"text": "hello tools"}`}}},
		{Content: "done"},
	}}
	agent := NewSmoothAgent(client, AgentOptions{Tools: []Tool{echo}})
	res, err := agent.Run(context.Background(), "use echo", nil)
	if err != nil {
		t.Fatal(err)
	}
	if res.Text != "done" || res.ToolCalls != 1 {
		t.Fatalf("unexpected result: %+v", res)
	}
	// The tool result was fed back as a tool-role message before the final call.
	second := client.calls[1].Messages
	found := false
	for _, m := range second {
		if m.Role == "tool" && m.Content == "hello tools" {
			found = true
		}
	}
	if !found {
		t.Fatalf("tool result not fed back; messages=%+v", second)
	}
}

func TestKnowledgeInjectedIntoSystemPrompt(t *testing.T) {
	kb := &InMemoryKnowledge{}
	kb.Ingest("The return window is exactly 17 days from delivery.", "returns.md")
	client := &fakeClient{scripted: []ChatResponse{{Content: "17 days"}}}
	agent := NewSmoothAgent(client, AgentOptions{Instructions: "support agent", Knowledge: kb})
	if _, err := agent.Run(context.Background(), "how many days to return?", nil); err != nil {
		t.Fatal(err)
	}
	first := client.calls[0].Messages
	if len(first) == 0 || first[0].Role != "system" || !strings.Contains(first[0].Content, "17 days") {
		t.Fatalf("knowledge not injected into system prompt; first=%+v", first)
	}
}
