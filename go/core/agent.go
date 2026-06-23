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
	Usage     Usage
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

// DelegateTool builds a Tool that delegates a subtask to a child SmoothAgent.
//
// A sub-agent is just a tool backed by another agent: the model calls this tool
// with a "task" argument, the child agent runs that task, and the child's final
// reply becomes the tool result — composing with the existing tool loop, no
// special wiring. The child can have its own instructions, tools, knowledge, etc.
func DelegateTool(name, description string, child *SmoothAgent) Tool {
	return FuncTool{
		ToolName: name,
		Desc:     description,
		Params: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"task": map[string]any{"type": "string", "description": "The subtask for the sub-agent to perform."},
			},
			"required": []string{"task"},
		},
		Fn: func(ctx context.Context, args map[string]any) (string, error) {
			task, _ := args["task"].(string)
			result, err := child.Run(ctx, task, nil)
			if err != nil {
				return "", err
			}
			return result.Text, nil
		},
	}
}

// AgentOptions configures a SmoothAgent turn. Mirrors the sibling cores' options.
type AgentOptions struct {
	Instructions  string
	Model         string
	MaxIterations int
	MaxTokens     int
	Temperature   float64
	Knowledge     Knowledge
	KnowledgeTopK int
	// Reranker reorders retrieved hits before injection (nil = passthrough).
	Reranker Reranker
	// KnowledgeCandidateK is the pool size retrieved before reranking; when greater
	// than KnowledgeTopK, more docs are fetched, reranked, then trimmed to TopK.
	KnowledgeCandidateK int
	// Memory, if set, recalls relevant facts into context each turn.
	Memory Memory
	// MemoryTopK is how many memory entries to recall per turn (0 = default 4).
	MemoryTopK int
	Tools      []Tool
	// MaxContextTokens is the approximate token budget for the context window.
	// Before each model call, older non-system messages are dropped (sliding
	// window) to stay under it. 0 uses the default (8000); negative disables.
	MaxContextTokens int
	// Budget, if set, stops the turn early once accumulated usage/cost hits it.
	Budget *CostBudget
	// Pricing overrides the per-model cost table (defaults to DefaultPricing).
	Pricing map[string]ModelPricing
	// CheckpointStore, with ConversationID, persists/resumes the conversation.
	CheckpointStore CheckpointStore
	// ConversationID keys the checkpoint store (required to use checkpointing).
	ConversationID string
	// Clearance, if set, gates which tools may be dispatched. A tool the clearance
	// forbids is not executed — a "tool not permitted" result is returned to the
	// model instead. Nil allows every tool (the prior behaviour).
	Clearance *Clearance
	// HumanGate, when set, is asked for approval before running any tool call for
	// which RequiresApproval returns true. A denied call is not executed; the model
	// is told it was denied and can adapt.
	HumanGate HumanGate
	// RequiresApproval reports which tool calls need human approval (e.g. writes /
	// destructive actions), given the tool name and parsed arguments. nil = none.
	// Only consulted when HumanGate is set. Example:
	//
	//	func(name string, _ map[string]any) bool { return name == "delete_record" }
	RequiresApproval func(name string, args map[string]any) bool
}

// AgentRunResponse is the result of a turn.
type AgentRunResponse struct {
	Text       string
	Iterations int
	ToolCalls  int
	Usage      Usage
	CostUSD    float64
	// BudgetExceeded is true if the turn stopped because the cost/token budget was hit.
	BudgetExceeded bool
}

const (
	defaultModel            = "claude-haiku-4-5"
	defaultMaxIterations    = 8
	defaultMaxTokens        = 512
	defaultKnowledgeTopK    = 4
	defaultMaxContextTokens = 8000
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

	if a.options.Memory != nil {
		topK := a.options.MemoryTopK
		if topK <= 0 {
			topK = defaultKnowledgeTopK
		}
		recalled := a.options.Memory.Recall(message, topK)
		if len(recalled) > 0 {
			lines := make([]string, len(recalled))
			for i, e := range recalled {
				lines[i] = "- " + e.Text
			}
			system = strings.TrimSpace(system + "\n\nRelevant memory (things you remember about this user/context):\n" + strings.Join(lines, "\n"))
		}
	}

	if a.options.Knowledge != nil {
		topK := a.options.KnowledgeTopK
		if topK <= 0 {
			topK = defaultKnowledgeTopK
		}
		candidateK := topK
		if a.options.KnowledgeCandidateK > candidateK {
			candidateK = a.options.KnowledgeCandidateK
		}
		hits := a.options.Knowledge.Query(message, candidateK)
		if a.options.Reranker != nil {
			hits = a.options.Reranker.Rerank(message, hits)
		}
		if len(hits) > topK {
			hits = hits[:topK]
		}
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
	return a.run(ctx, message, history, nil)
}

// RunThread executes a single turn carried by a SmoothAgentThread: the turn is seeded
// from the thread's messages, and this turn's new user + assistant (+ tool) messages
// are appended back to the thread before returning. The thread takes precedence over
// any history as the prior context. Run (single-shot/history) keeps working unchanged.
func (a *SmoothAgent) RunThread(ctx context.Context, message string, thread *SmoothAgentThread) (AgentRunResponse, error) {
	return a.run(ctx, message, nil, thread)
}

func (a *SmoothAgent) run(ctx context.Context, message string, history []ChatMessage, thread *SmoothAgentThread) (AgentRunResponse, error) {
	messages := make([]ChatMessage, 0, len(history)+2)
	if system := a.buildSystem(message); system != "" {
		messages = append(messages, ChatMessage{Role: "system", Content: system})
	}

	// Source prior conversation: the thread (if passed) wins, then the checkpoint
	// store (if configured), then the explicit history argument.
	cpStore := a.options.CheckpointStore
	cpID := a.options.ConversationID
	prior := history
	if cpStore != nil && cpID != "" {
		if loaded, ok := cpStore.Load(cpID); ok {
			prior = loaded.Messages
		}
	}
	if thread != nil {
		prior = thread.Messages()
	}
	messages = append(messages, prior...)
	messages = append(messages, ChatMessage{Role: "user", Content: message})

	// Track this turn's new messages (user + assistant + tool, never system) so they
	// can be appended back to the thread on exit. Slicing the live messages by index
	// would be unsafe — compaction may drop/reorder it mid-turn.
	turnMessages := []ChatMessage{{Role: "user", Content: message}}

	// Persist the conversation (sans system prompt, rebuilt each turn) on any exit,
	// and append this turn's messages to the thread.
	defer func() {
		if cpStore != nil && cpID != "" {
			nonSystem := make([]ChatMessage, 0, len(messages))
			for _, m := range messages {
				if m.Role != "system" {
					nonSystem = append(nonSystem, m)
				}
			}
			cpStore.Save(Checkpoint{ConversationID: cpID, Messages: nonSystem})
		}
		if thread != nil {
			thread.Extend(turnMessages)
		}
	}()

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
	maxContext := a.options.MaxContextTokens
	if maxContext == 0 {
		maxContext = defaultMaxContextTokens
	}

	toolCalls := 0
	lastText := ""
	var tracker CostTracker

	for iteration := 1; iteration <= maxIter; iteration++ {
		// Keep the context window within budget before each model call.
		messages = compact(messages, maxContext)
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
		tracker.Record(model, resp.Usage, a.options.Pricing)
		lastText = resp.Content

		assistantMsg := ChatMessage{Role: "assistant", Content: resp.Content, ToolCalls: resp.ToolCalls}
		messages = append(messages, assistantMsg)
		turnMessages = append(turnMessages, assistantMsg)

		// Stop early if this turn has hit its token/cost budget.
		if tracker.Exceeds(a.options.Budget) {
			return AgentRunResponse{Text: lastText, Iterations: iteration, ToolCalls: toolCalls, Usage: tracker.Usage, CostUSD: tracker.CostUSD, BudgetExceeded: true}, nil
		}

		if len(resp.ToolCalls) == 0 {
			return AgentRunResponse{Text: lastText, Iterations: iteration, ToolCalls: toolCalls, Usage: tracker.Usage, CostUSD: tracker.CostUSD}, nil
		}

		for _, tc := range resp.ToolCalls {
			toolCalls++
			result := a.dispatchTool(ctx, tc)
			toolMsg := ChatMessage{Role: "tool", ToolCallID: tc.ID, Content: result}
			messages = append(messages, toolMsg)
			turnMessages = append(turnMessages, toolMsg)
		}
	}

	return AgentRunResponse{Text: lastText, Iterations: maxIter, ToolCalls: toolCalls, Usage: tracker.Usage, CostUSD: tracker.CostUSD}, nil
}

func (a *SmoothAgent) dispatchTool(ctx context.Context, tc ToolCall) string {
	// Enforce the role's tool clearance before dispatch: a forbidden tool is never
	// executed — the model is told it isn't permitted, mirroring how the loop
	// surfaces other tool errors.
	if a.options.Clearance != nil && !a.options.Clearance.IsAllowed(tc.Name) {
		return fmt.Sprintf("error: tool '%s' is not permitted for this role", tc.Name)
	}

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

	// Human-in-the-loop: pause for approval before running a flagged (write/sensitive)
	// tool. A denial is fed back to the model as a result — the tool never runs.
	if a.options.HumanGate != nil && a.options.RequiresApproval != nil && a.options.RequiresApproval(tc.Name, args) {
		req := HumanApprovalRequest{ToolName: tc.Name, Arguments: args, Prompt: fmt.Sprintf("Approve calling tool '%s'?", tc.Name)}
		decision, err := a.options.HumanGate(ctx, req)
		if err != nil {
			return fmt.Sprintf("error: human gate for tool '%s' failed: %v", tc.Name, err)
		}
		if !decision.IsApproved() {
			reason := decision.Reason
			if reason == "" {
				reason = "no reason given"
			}
			return fmt.Sprintf("Denied by human: %s", reason)
		}
	}

	out, err := tool.Execute(ctx, args)
	if err != nil {
		// Surface tool failures to the model, don't crash the turn.
		return fmt.Sprintf("error: tool '%s' failed: %v", tc.Name, err)
	}
	return out
}
