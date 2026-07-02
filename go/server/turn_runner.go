package server

import (
	"context"
	"encoding/json"
	"errors"
	"strings"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// errNoEngine is returned by a turn when no chat client (LLM gateway) is configured.
var errNoEngine = errors.New("server: no chat engine configured")

const (
	maxPriorMessages       = 50
	defaultSystemPrompt    = "You are a helpful customer support agent. Answer using only the knowledge provided to you; if it is not there, say you don't know."
	citationSnippetMaxChar = 280
	// autoContextLimit is the top-K the runner queries the knowledge base with to
	// build the turn's auto-context citations — the same query the engine runs to
	// auto-inject grounding (`query(message, 3)`), so the citations mirror the sources
	// the model was actually grounded on. Matches the Rust AUTO_CONTEXT_LIMIT and the
	// TS server's AUTO_CONTEXT_LIMIT.
	autoContextLimit = 3
)

// TurnResult is what a completed turn produced — the Go analog of the C# TurnResult
// and the Rust TurnResult.
type TurnResult struct {
	Reply     string
	MessageID string
	Citations []Citation
	// NextStepID is the workflow step id this conversation should resume on next turn,
	// after the post-turn judge. Non-empty only when a workflow was configured; the
	// caller (dispatcher) persists it. "" for freeform agents. SMOODEV-590.
	NextStepID string
}

// EventSink writes one built protocol event frame to the connection. Handlers/runners
// emit through it so a streaming turn can fire many stream_token events while the
// connection is still reading (the Rust sink_tx / C# channel split).
type EventSink func(event map[string]any)

// TurnRunner drives one send_message turn: load prior history into a thread, build the
// engine agent, consume RunStream, emit a stream_token per text delta and a
// stream_chunk per tool call / tool result, persist the reply, and return the
// citations. The Go analog of the C# TurnRunner / Rust run_streaming_turn.
type TurnRunner struct {
	client       core.ChatClient
	store        SessionStore
	systemPrompt string
	tools        []core.Tool
	// knowledge is the retriever (already SCOPED to the connection's access) the agent
	// grounds on. When set, the runner also queries it with the user message (top
	// autoContextLimit) to build the turn's auto-context citations — the sources the
	// engine's grounding query surfaced. nil → no grounding, citations empty (parity
	// with a turn that retrieves nothing).
	knowledge core.Knowledge
	// confirmTools are tool-name substrings gated behind write-confirmation HITL
	// (empty → HITL off, behavior unchanged). When a turn calls a tool whose name
	// contains one of these, the runner parks the turn and emits
	// write_confirmation_required until the client replies with confirm_tool_action.
	confirmTools []string
	// confirmations is the session-keyed pending-confirmation registry shared with
	// the dispatcher so a confirm_tool_action frame resolves the verdict a parked
	// turn awaits. nil → HITL off.
	confirmations *ConfirmationRegistry
	// workflow is the agent's structured conversation workflow (nil → freeform). When
	// set, the runner judges the turn after it completes and returns the advanced step
	// id in TurnResult.NextStepID. The current step is already rendered into systemPrompt
	// by the caller (via assembleSystemPrompt). SMOODEV-590.
	workflow *ConversationWorkflow
	// currentStepID is the conversation's current workflow step id — the pointer the
	// post-turn judge advances from.
	currentStepID string
	// judgeModel is the cheap model id the workflow judge uses ("" → DefaultJudgeModel).
	judgeModel string
}

// NewTurnRunner builds a runner over the engine chat client + session store. An empty
// systemPrompt falls back to the default support-agent prompt. tools (default none) are
// passed straight to the engine AgentOptions so the agent can call them mid-turn.
// knowledge (default nil) grounds the agent AND sources the turn's auto-context
// citations. confirmTools + confirmations wire write-confirmation HITL; pass nil/empty
// to disable.
// The systemPrompt is already assembled (base + per-agent config + current workflow step)
// and tools already filtered by the caller. workflow/currentStepID/judgeModel (all
// zero-valued by default) drive the post-turn workflow judge; a nil workflow disables it,
// so behavior is unchanged for freeform agents. SMOODEV-590.
func NewTurnRunner(client core.ChatClient, store SessionStore, systemPrompt string, knowledge core.Knowledge, tools []core.Tool, confirmTools []string, confirmations *ConfirmationRegistry, workflow *ConversationWorkflow, currentStepID, judgeModel string) *TurnRunner {
	if systemPrompt == "" {
		systemPrompt = defaultSystemPrompt
	}
	return &TurnRunner{
		client:        client,
		store:         store,
		systemPrompt:  systemPrompt,
		knowledge:     knowledge,
		tools:         tools,
		confirmTools:  confirmTools,
		confirmations: confirmations,
		workflow:      workflow,
		currentStepID: currentStepID,
		judgeModel:    judgeModel,
	}
}

// isGated reports whether toolName matches a confirmation-gated pattern (substring,
// like the Rust ConfirmationHook + the Python gate). Only meaningful when a
// confirmation registry is wired.
func (r *TurnRunner) isGated(toolName string) bool {
	if r.confirmations == nil {
		return false
	}
	for _, pattern := range r.confirmTools {
		if strings.Contains(toolName, pattern) {
			return true
		}
	}
	return false
}

// Run streams one turn for conversationID keyed on requestID, emitting events through
// sink, and returns the settled TurnResult. sessionID keys the write-confirmation
// registry (so a confirm_tool_action for the same session resumes this turn). A nil
// client (no engine configured) surfaces to the caller as an error so the handler can
// emit a clean protocol error.
func (r *TurnRunner) Run(ctx context.Context, sessionID, conversationID, requestID, userMessage string, sink EventSink) (TurnResult, error) {
	// No engine configured (the keyless / no-gateway path): fail with a clear error the
	// dispatcher turns into a clean protocol error, rather than letting the engine's
	// NewSmoothAgent panic on a nil client.
	if r.client == nil {
		return TurnResult{}, errNoEngine
	}

	// 1. Auto-context citations (what grounded the answer). Mirrors the Rust auto_sources
	//    / TS citation build: query the same retriever the engine grounds on with the user
	//    message (top autoContextLimit), so the citations match the sources the model
	//    actually saw. Built BEFORE the stream so the terminal eventual_response carries
	//    them. nil knowledge → no citations (parity with a turn that retrieves nothing).
	var citations []Citation
	if r.knowledge != nil {
		for _, hit := range r.knowledge.Query(userMessage, autoContextLimit) {
			isURL := strings.HasPrefix(hit.Source, "http://") || strings.HasPrefix(hit.Source, "https://")
			c := Citation{
				ID:      hit.Source,
				Title:   hit.Source,
				Snippet: truncate(hit.Content, citationSnippetMaxChar),
				Score:   hit.Score,
			}
			if isURL {
				c.URL = hit.Source
			}
			citations = append(citations, c)
		}
	}

	// 2. Build the agent + replay prior history into a thread (before persisting this
	//    turn's inbound message, so the thread doesn't double-count it). The same
	//    knowledge feeds the engine's grounding so its auto-injected context matches the
	//    citations built above. r.systemPrompt is already assembled (base + per-agent
	//    config + current workflow step) by the caller.
	opts := core.AgentOptions{Instructions: r.systemPrompt, Tools: r.tools, Knowledge: r.knowledge}

	// Write-confirmation HITL: when configured with tool patterns AND a registry is
	// present, install a HumanGate that parks the turn before a gated tool runs (emit
	// write_confirmation_required, await the client's verdict via the session-keyed
	// registry). With no patterns (the default) no gate is installed → no tool ever
	// parks → behavior identical to before HITL. The gate keys its pending channel by
	// sessionID, so a confirm_tool_action frame (also keyed by sessionId) routes back
	// here.
	//
	// Event ORDER matters for cross-language parity: the reference (Rust) server emits
	// write_confirmation_required BEFORE the gated tool's stream_chunk(toolCall). The
	// engine, however, yields the StreamToolCall event before consulting the gate — so
	// the stream loop below DEFERS a gated tool's stream_chunk (see isGated) and the
	// gate emits it HERE, right after the confirmation prompt, to match.
	if len(r.confirmTools) > 0 && r.confirmations != nil {
		opts.RequiresApproval = func(name string, _ map[string]any) bool {
			return r.isGated(name)
		}
		opts.HumanGate = func(gateCtx context.Context, req core.HumanApprovalRequest) (core.HumanApprovalResponse, error) {
			// Park: register a fresh verdict channel, emit the confirmation event +
			// the deferred toolCall chunk, then await the client's confirm_tool_action.
			// The toolId is the tool name (one tool parks at a time — a stable
			// correlation key); actionDescription is the engine's prompt.
			verdict := r.confirmations.Register(sessionID)
			argsJSON, err := json.Marshal(req.Arguments)
			if err != nil {
				argsJSON = []byte("{}")
			}
			sink(writeConfirmationRequired(requestID, req.ToolName, req.Prompt))
			sink(streamChunk(requestID, req.ToolName, toolCallState(req.ToolName, string(argsJSON))))
			select {
			case approved := <-verdict:
				if approved {
					return core.Approve(), nil
				}
				return core.Deny("user rejected the action"), nil
			case <-gateCtx.Done():
				// The turn's context was cancelled (connection torn down before a
				// verdict landed) — fail closed: deny, never auto-approve a write.
				return core.Deny("connection closed before confirmation"), gateCtx.Err()
			}
		}
	}

	agent := core.NewSmoothAgent(r.client, opts)
	thread := core.NewThread()
	prior, err := r.store.ListMessages(ctx, conversationID, maxPriorMessages)
	if err != nil {
		return TurnResult{}, err
	}
	for _, m := range prior {
		role := "user"
		if m.Direction == Outbound {
			role = "assistant"
		}
		thread.Add(core.ChatMessage{Role: role, Content: m.Text})
	}

	// 3. Persist the inbound user message.
	if _, err := r.store.AppendMessage(ctx, conversationID, Inbound, userMessage); err != nil {
		return TurnResult{}, err
	}

	// 4. Stream the turn: a stream_token per text delta, a stream_chunk per tool call /
	//    tool result (mirrors the Rust runner translating StreamToolCall/StreamToolResult
	//    events and the C# RunStreamingAsync loop).
	stream, err := agent.RunStream(ctx, userMessage, thread)
	if err != nil {
		return TurnResult{}, err
	}
	// Turn over: drop any lingering pending confirmation so a stale entry can't
	// mis-route a later confirm_tool_action (mirrors the Rust (cfg.clear)(session_id)
	// at turn end). No-op when HITL is off.
	if r.confirmations != nil {
		defer r.confirmations.Clear(sessionID)
	}
	var reply strings.Builder
	for ev := range stream.Events() {
		switch ev.Kind {
		case core.StreamText:
			if ev.Text != "" {
				reply.WriteString(ev.Text)
				sink(streamToken(requestID, ev.Text))
			}
		case core.StreamToolCall:
			// DEFER a confirmation-gated tool's toolCall chunk: it is emitted from the
			// gate AFTER write_confirmation_required, so the wire order matches the
			// reference (Rust) server. Non-gated tools emit their chunk inline as before.
			if r.isGated(ev.Name) {
				continue
			}
			sink(streamChunk(requestID, ev.Name, toolCallState(ev.Name, ev.Arguments)))
		case core.StreamToolResult:
			sink(streamChunk(requestID, ev.Name, toolResultState(ev.Name, ev.Result)))
		case core.StreamDone:
			// The terminal AgentRunResponse; the eventual_response is built by the
			// dispatcher from the accumulated reply, so nothing to emit here.
		}
	}
	// A model-call error aborts the stream WITHOUT a StreamDone; surface it so the
	// turn settles as a protocol error rather than an empty success.
	if err := stream.Err(); err != nil {
		return TurnResult{}, err
	}

	// 5. Persist the outbound reply.
	outbound, err := r.store.AppendMessage(ctx, conversationID, Outbound, reply.String())
	if err != nil {
		return TurnResult{}, err
	}

	// 6. Post-turn workflow judge (SMOODEV-590). When the agent has a structured
	//    workflow, a cheap judge call decides whether the current step's criteria were met
	//    this turn and advances the pointer; the advanced step id is returned so the
	//    caller persists it. Failure-tolerant: any judge error keeps the conversation on
	//    the current step (never freezes / skips). No-op for freeform agents (NextStepID
	//    stays "").
	var nextStepID string
	if r.workflow != nil {
		verdict := judgeWorkflowStep(ctx, r.client, r.judgeModel, r.workflow, r.currentStepID, userMessage, reply.String())
		nextStepID = advanceStep(r.workflow, r.currentStepID, verdict)
	}

	return TurnResult{Reply: reply.String(), MessageID: outbound.ID, Citations: citations, NextStepID: nextStepID}, nil
}

// truncate caps a citation snippet at max characters (a plain prefix slice, matching
// the TS server's truncate). The seeded chunks are short, so this is a no-op for the
// conformance corpus; it bounds the wire size for real documents.
func truncate(value string, max int) string {
	if len(value) <= max {
		return value
	}
	return value[:max]
}

// toolCallState builds the stream_chunk state for a requested tool call, matching the
// Rust/C# rawResponse.toolCall shape. arguments is the raw JSON string the model emitted.
func toolCallState(name, arguments string) map[string]any {
	var parsed any
	if arguments != "" {
		if err := json.Unmarshal([]byte(arguments), &parsed); err != nil {
			parsed = map[string]any{}
		}
	} else {
		parsed = map[string]any{}
	}
	return map[string]any{
		"rawResponse": map[string]any{
			"toolCall": map[string]any{"name": name, "arguments": parsed},
		},
	}
}

// toolResultState builds the stream_chunk state for a tool result, matching the
// Rust/C# rawResponse.toolResult shape. The engine folds tool failures into the
// result string, so detect that convention for the isError flag.
func toolResultState(name, result string) map[string]any {
	isError := strings.HasPrefix(result, "Error:") || strings.HasPrefix(result, "Denied by human:")
	return map[string]any{
		"rawResponse": map[string]any{
			"toolResult": map[string]any{"name": name, "isError": isError, "result": result},
		},
	}
}
