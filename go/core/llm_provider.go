package core

import (
	"context"
	"errors"
)

// LlmProvider is the LLM-call seam the agent loop depends on. It is the existing
// ChatClient interface under a name that names the role — formalizing the seam so
// the agent loop is unit-testable deterministically, without a live model or
// network. The GatewayClient already implements it; tests inject a MockLlmProvider.
//
// This keeps backward compatibility: NewSmoothAgent still takes a ChatClient, and
// any LlmProvider is a ChatClient (and vice versa) since they are the same shape.
type LlmProvider = ChatClient

// TextResponse builds a plain-text ChatResponse (no tool calls). Handy for
// scripting the mock and for assertions.
func TextResponse(content string) ChatResponse {
	return ChatResponse{Content: content}
}

// ToolCallResponse builds a ChatResponse that requests a single tool call.
// arguments is the raw JSON-string the model emits for the call's arguments.
func ToolCallResponse(id, name, arguments string) ChatResponse {
	return ChatResponse{ToolCalls: []ToolCall{{ID: id, Name: name, Arguments: arguments}}}
}

// scriptedOutcome is one entry in the mock's script: a response or an error.
type scriptedOutcome struct {
	resp ChatResponse
	err  error
}

// MockLlmProvider is a deterministic LlmProvider for tests. Script the responses
// it should return (FIFO), drive your code, then assert on Calls. Build it up
// fluently with PushText / PushToolCall / PushError.
//
// It replaces the ad-hoc fakeClient the tests rolled by hand, and mirrors the
// BEHAVIOR of the Rust reference's MockLlmClient (record + replay). It is not
// safe for concurrent use — a turn drives it serially, which is the intended use.
type MockLlmProvider struct {
	script []scriptedOutcome
	calls  []ChatRequest
}

// NewMockLlmProvider returns an empty mock. Script it with the Push* methods.
func NewMockLlmProvider() *MockLlmProvider {
	return &MockLlmProvider{}
}

// PushResponse queues a raw ChatResponse for the next Chat call.
func (m *MockLlmProvider) PushResponse(resp ChatResponse) *MockLlmProvider {
	m.script = append(m.script, scriptedOutcome{resp: resp})
	return m
}

// PushText queues a plain-text response for the next Chat call.
func (m *MockLlmProvider) PushText(content string) *MockLlmProvider {
	return m.PushResponse(TextResponse(content))
}

// PushToolCall queues a single-tool-call response for the next Chat call.
func (m *MockLlmProvider) PushToolCall(id, name, arguments string) *MockLlmProvider {
	return m.PushResponse(ToolCallResponse(id, name, arguments))
}

// PushError queues an error to be returned from the next Chat call.
func (m *MockLlmProvider) PushError(message string) *MockLlmProvider {
	m.script = append(m.script, scriptedOutcome{err: errors.New(message)})
	return m
}

// Calls returns every request the mock has received so far, in order.
func (m *MockLlmProvider) Calls() []ChatRequest {
	return m.calls
}

// CallCount returns the number of requests received.
func (m *MockLlmProvider) CallCount() int {
	return len(m.calls)
}

// LastCall returns the most recent request and true, or a zero request and false
// if none have been received.
func (m *MockLlmProvider) LastCall() (ChatRequest, bool) {
	if len(m.calls) == 0 {
		return ChatRequest{}, false
	}
	return m.calls[len(m.calls)-1], true
}

// Chat implements ChatClient / LlmProvider: it records the request, then replays
// the next scripted outcome. With an empty script it returns a benign empty text
// response so loops terminate cleanly.
func (m *MockLlmProvider) Chat(_ context.Context, req ChatRequest) (ChatResponse, error) {
	m.calls = append(m.calls, req)
	if len(m.script) == 0 {
		return ChatResponse{}, nil
	}
	next := m.script[0]
	m.script = m.script[1:]
	if next.err != nil {
		return ChatResponse{}, next.err
	}
	return next.resp, nil
}
