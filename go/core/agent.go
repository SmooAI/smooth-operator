package core

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
)

// ToolCall is a model-requested tool invocation.
type ToolCall struct {
	ID        string
	Name      string
	Arguments string // raw JSON
}

// ChatMessage is one message in the OpenAI-shaped conversation.
type ChatMessage struct {
	Role       string
	Content    string
	ToolCalls  []ToolCall
	ToolCallID string // set on role=="tool" messages
}

// ToolSpec is a tool advertised to the model.
type ToolSpec struct {
	Name        string
	Description string
	Parameters  map[string]any // JSON Schema
}

// ChatRequest is a single model call.
type ChatRequest struct {
	Model       string
	Messages    []ChatMessage
	Tools       []ToolSpec
	Temperature float64
	MaxTokens   int
}

// ChatResponse is the assistant's reply (content and/or tool calls).
type ChatResponse struct {
	Content   string
	ToolCalls []ToolCall
}

// ChatClient is the minimal OpenAI-compatible surface the agent needs. The
// GatewayClient implements it against a live endpoint; tests inject a fake.
type ChatClient interface {
	Chat(ctx context.Context, req ChatRequest) (ChatResponse, error)
}

// Tool is a callable the agent may invoke.
type Tool interface {
	Name() string
	Description() string
	Parameters() map[string]any
	Execute(ctx context.Context, args map[string]any) (string, error)
}

// FuncTool wraps a function as a Tool (the AIFunctionFactory analogue).
type FuncTool struct {
	ToolName string
	Desc     string
	Params   map[string]any
	Fn       func(ctx context.Context, args map[string]any) (string, error)
}

func (t FuncTool) Name() string               { return t.ToolName }
func (t FuncTool) Description() string        { return t.Desc }
func (t FuncTool) Parameters() map[string]any { return t.Params }
func (t FuncTool) Execute(ctx context.Context, args map[string]any) (string, error) {
	return t.Fn(ctx, args)
}

// AgentOptions configures a SmoothAgent turn. Mirrors the sibling cores' options.
type AgentOptions struct {
	Instructions  string
	Model         string
	MaxIterations int
	MaxTokens     int
	Temperature   float64
	Knowledge     *InMemoryKnowledge
	KnowledgeTopK int
	Tools         []Tool
}

// AgentRunResponse is the result of a turn.
type AgentRunResponse struct {
	Text       string
	Iterations int
	ToolCalls  int
}

const (
	defaultModel         = "claude-haiku-4-5"
	defaultMaxIterations = 8
	defaultMaxTokens     = 512
	defaultKnowledgeTopK = 4
)

// SmoothAgent is a native, in-process agent.
type SmoothAgent struct {
	client      ChatClient
	options     AgentOptions
	toolsByName map[string]Tool
}

// NewSmoothAgent constructs an agent over an OpenAI-compatible ChatClient.
func NewSmoothAgent(client ChatClient, options AgentOptions) *SmoothAgent {
	if client == nil {
		panic("core: client is required")
	}
	byName := make(map[string]Tool, len(options.Tools))
	for _, t := range options.Tools {
		byName[t.Name()] = t
	}
	return &SmoothAgent{client: client, options: options, toolsByName: byName}
}

func (a *SmoothAgent) buildSystem(message string) string {
	system := a.options.Instructions
	if a.options.Knowledge != nil {
		topK := a.options.KnowledgeTopK
		if topK <= 0 {
			topK = defaultKnowledgeTopK
		}
		hits := a.options.Knowledge.Query(message, topK)
		if len(hits) > 0 {
			parts := make([]string, len(hits))
			for i, h := range hits {
				parts[i] = fmt.Sprintf("[%s] %s", h.Source, h.Content)
			}
			block := strings.Join(parts, "\n\n")
			system = strings.TrimSpace(system + "\n\nKnowledge base (ground all facts ONLY in this; if it is not here, say you don't know):\n" + block)
		}
	}
	return system
}

func (a *SmoothAgent) toolSpecs() []ToolSpec {
	if len(a.options.Tools) == 0 {
		return nil
	}
	specs := make([]ToolSpec, len(a.options.Tools))
	for i, t := range a.options.Tools {
		specs[i] = ToolSpec{Name: t.Name(), Description: t.Description(), Parameters: t.Parameters()}
	}
	return specs
}

// Run executes a single turn. history is prior conversation messages (multi-turn).
func (a *SmoothAgent) Run(ctx context.Context, message string, history []ChatMessage) (AgentRunResponse, error) {
	messages := make([]ChatMessage, 0, len(history)+2)
	if system := a.buildSystem(message); system != "" {
		messages = append(messages, ChatMessage{Role: "system", Content: system})
	}
	messages = append(messages, history...)
	messages = append(messages, ChatMessage{Role: "user", Content: message})

	model := a.options.Model
	if model == "" {
		model = defaultModel
	}
	maxIter := a.options.MaxIterations
	if maxIter <= 0 {
		maxIter = defaultMaxIterations
	}
	maxTokens := a.options.MaxTokens
	if maxTokens <= 0 {
		maxTokens = defaultMaxTokens
	}
	tools := a.toolSpecs()

	toolCalls := 0
	lastText := ""

	for iteration := 1; iteration <= maxIter; iteration++ {
		resp, err := a.client.Chat(ctx, ChatRequest{
			Model:       model,
			Messages:    messages,
			Tools:       tools,
			Temperature: a.options.Temperature,
			MaxTokens:   maxTokens,
		})
		if err != nil {
			return AgentRunResponse{}, fmt.Errorf("model call: %w", err)
		}
		lastText = resp.Content

		messages = append(messages, ChatMessage{Role: "assistant", Content: resp.Content, ToolCalls: resp.ToolCalls})

		if len(resp.ToolCalls) == 0 {
			return AgentRunResponse{Text: lastText, Iterations: iteration, ToolCalls: toolCalls}, nil
		}

		for _, tc := range resp.ToolCalls {
			toolCalls++
			result := a.dispatchTool(ctx, tc)
			messages = append(messages, ChatMessage{Role: "tool", ToolCallID: tc.ID, Content: result})
		}
	}

	return AgentRunResponse{Text: lastText, Iterations: maxIter, ToolCalls: toolCalls}, nil
}

func (a *SmoothAgent) dispatchTool(ctx context.Context, tc ToolCall) string {
	tool, ok := a.toolsByName[tc.Name]
	if !ok {
		return fmt.Sprintf("error: unknown tool '%s'", tc.Name)
	}
	args := map[string]any{}
	if tc.Arguments != "" {
		if err := json.Unmarshal([]byte(tc.Arguments), &args); err != nil {
			return fmt.Sprintf("error: tool '%s' received invalid JSON arguments", tc.Name)
		}
	}
	out, err := tool.Execute(ctx, args)
	if err != nil {
		// Surface tool failures to the model, don't crash the turn.
		return fmt.Sprintf("error: tool '%s' failed: %v", tc.Name, err)
	}
	return out
}
