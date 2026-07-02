package server

import (
	"context"
	"encoding/json"
	"sync"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/google/uuid"
)

// FrameDispatcher routes one inbound protocol frame (by its action discriminator) to
// the right handler and emits the response event(s) to the sink. The Go analog of the
// C# FrameDispatcher and the Rust handle_frame. Transport-agnostic: the WS server
// calls Dispatch per inbound text frame and the sink writes events back over the
// socket. One dispatcher is bound to one connection's AccessContext (resolved from the
// ?token= slot at connect).
type FrameDispatcher struct {
	store   SessionStore
	client  core.ChatClient
	access  AccessContext
	systemP string
	// knowledge is the retriever the agent grounds on (nil → no grounding). Threaded
	// into every turn the runner builds; it both grounds the engine and sources the
	// turn's auto-context citations.
	knowledge core.Knowledge
	tools     []core.Tool
	// confirmTools are tool-name substrings gated behind write-confirmation HITL
	// (empty → HITL off). Threaded into every turn the runner builds.
	confirmTools []string
	// confirmations is the per-connection session-keyed pending-confirmation registry.
	// A confirm_tool_action frame resolves the verdict the parked turn awaits. Shared
	// with the turn runner. Created on demand (one per connection) when HITL is enabled.
	confirmations *ConfirmationRegistry
	// agentConfigs resolves per-agent config (instructions, workflow, greeting,
	// personality, tool allow-list) by the session's agent id (SMOODEV-590). nil → the
	// built-in default prompt + full tool set, no workflow.
	agentConfigs AgentConfigResolver
	// judgeModel is the cheap model the workflow judge uses ("" → DefaultJudgeModel),
	// forwarded to every turn runner.
	judgeModel string
	// authRequiringTools is the set of tool names that support the per-agent authLevel gate.
	authRequiringTools map[string]bool
	// sessionAuth verifies end-user identity for end_user-gated tools (nil → fail-closed).
	sessionAuth SessionAuthenticator
	// turns tracks in-flight spawned send_message turns so the connection loop can wait
	// for them to finish (and flush their eventual_response) on teardown — the
	// graceful-drain contract. send_message runs its turn as a goroutine (so the read
	// loop stays free to receive a confirm_tool_action while a turn is parked).
	turns sync.WaitGroup
}

// NewFrameDispatcher binds a dispatcher to a connection's stores + access context. The
// knowledge retriever (default nil) and tools (default none) are threaded into every
// turn the runner builds. confirmTools + confirmations wire write-confirmation HITL;
// pass nil/empty + a registry to disable.
func NewFrameDispatcher(store SessionStore, client core.ChatClient, access AccessContext, systemPrompt string, knowledge core.Knowledge, tools []core.Tool, confirmTools []string, confirmations *ConfirmationRegistry, agentConfigs AgentConfigResolver, judgeModel string, authRequiringTools map[string]bool, sessionAuth SessionAuthenticator) *FrameDispatcher {
	if confirmations == nil {
		confirmations = NewConfirmationRegistry()
	}
	return &FrameDispatcher{
		store:              store,
		client:             client,
		access:             access,
		systemP:            systemPrompt,
		knowledge:          knowledge,
		tools:              tools,
		confirmTools:       confirmTools,
		confirmations:      confirmations,
		agentConfigs:       agentConfigs,
		judgeModel:         judgeModel,
		authRequiringTools: authRequiringTools,
		sessionAuth:        sessionAuth,
	}
}

// WaitForTurns blocks until every in-flight spawned send_message turn has completed.
// send_message runs its turn as a background goroutine (so the read loop stays free to
// receive a confirm_tool_action while a turn is parked); the connection loop calls this
// in its teardown so an in-flight turn finishes — and its eventual_response is flushed —
// before the writer stops and the backplane detach runs (the graceful-drain contract).
func (d *FrameDispatcher) WaitForTurns() { d.turns.Wait() }

// inboundFrame is the minimal envelope shared by every client→server action.
type inboundFrame struct {
	Action    string `json:"action"`
	RequestID string `json:"requestId"`
	// create_conversation_session
	AgentID   string `json:"agentId"`
	UserName  string `json:"userName"`
	UserEmail string `json:"userEmail"`
	// get_session / send_message / confirm_tool_action
	SessionID string `json:"sessionId"`
	Message   string `json:"message"`
	// confirm_tool_action — *bool so a missing verdict is distinguishable from
	// false (fail closed: a missing/garbled approved must NOT silently approve).
	Approved *bool `json:"approved"`
}

// Dispatch parses one raw frame and routes it. A handler failure mid-turn emits a
// clean error event and KEEPS the connection alive — the socket is never dropped with
// no signal to the client.
func (d *FrameDispatcher) Dispatch(ctx context.Context, raw []byte, sink EventSink) {
	var frame inboundFrame
	if err := json.Unmarshal(raw, &frame); err != nil {
		sink(errorEvent("", "VALIDATION_ERROR", "Invalid JSON frame"))
		return
	}

	switch frame.Action {
	case "ping":
		sink(pong(frame.RequestID))
	case "create_conversation_session":
		d.handleCreateSession(ctx, frame, sink)
	case "get_session":
		d.handleGetSession(ctx, frame, sink)
	case "send_message":
		d.handleSendMessage(ctx, frame, sink)
	case "confirm_tool_action":
		d.handleConfirmToolAction(frame, sink)
	case "":
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "Missing 'action'"))
	default:
		sink(errorEvent(frame.RequestID, "UNSUPPORTED_ACTION", "Unsupported action '"+frame.Action+"'"))
	}
}

func (d *FrameDispatcher) handleCreateSession(ctx context.Context, frame inboundFrame, sink EventSink) {
	session, err := d.store.CreateSession(ctx, frame.AgentID, frame.UserName, frame.UserEmail)
	if err != nil {
		sink(errorEvent(frame.RequestID, "INTERNAL_ERROR", "Internal error processing the request."))
		return
	}
	data := map[string]any{
		"sessionId":          session.SessionID,
		"conversationId":     session.ConversationID,
		"agentId":            session.AgentID,
		"agentName":          session.AgentName,
		"userParticipantId":  session.UserParticipantID,
		"agentParticipantId": session.AgentParticipantID,
	}
	sink(immediateResponse(frame.RequestID, 200, "Session created", data))
}

func (d *FrameDispatcher) handleGetSession(ctx context.Context, frame inboundFrame, sink EventSink) {
	session, err := d.store.GetSession(ctx, frame.SessionID)
	if err != nil {
		sink(errorEvent(frame.RequestID, "INTERNAL_ERROR", "Internal error processing the request."))
		return
	}
	if session == nil {
		sink(errorEvent(frame.RequestID, "SESSION_NOT_FOUND", "Session not found"))
		return
	}
	data := map[string]any{
		"sessionId":      session.SessionID,
		"conversationId": session.ConversationID,
		"agentId":        session.AgentID,
		"agentName":      session.AgentName,
	}
	sink(immediateResponse(frame.RequestID, 200, "OK", data))
}

func (d *FrameDispatcher) handleSendMessage(ctx context.Context, frame inboundFrame, sink EventSink) {
	requestID := frame.RequestID
	if requestID == "" {
		requestID = uuid.NewString()
	}
	session, err := d.store.GetSession(ctx, frame.SessionID)
	if err != nil {
		sink(errorEvent(requestID, "INTERNAL_ERROR", "Internal error processing the request."))
		return
	}
	if session == nil {
		sink(errorEvent(requestID, "SESSION_NOT_FOUND", "Session not found"))
		return
	}

	// Resolve this agent's per-agent config (instructions, conversation workflow,
	// greeting, personality, tool allow-list) by the session's agent id, and fold it into
	// the effective system prompt + tools for THIS turn (SMOODEV-590). An un-configured
	// agent (no resolver / nil config) falls back to the server default prompt + full tool
	// set — behavior unchanged. Resolution never fails the turn: a resolver error degrades
	// to the default.
	var agentConfig *AgentConfig
	if d.agentConfigs != nil {
		if cfg, cfgErr := d.agentConfigs.Resolve(ctx, session.AgentID); cfgErr == nil {
			agentConfig = cfg
		}
	}
	// First turn (server-side, from prior history) gates the greeting section — applied
	// only on turn 1, matching the Python sibling. Checked before the runner persists this
	// turn's inbound message, so an empty log means "no prior reply yet".
	prior, _ := d.store.ListMessages(ctx, session.ConversationID, 1)
	isFirstTurn := len(prior) == 0
	effectiveSystemPrompt := assembleSystemPrompt(d.systemP, agentConfig, session.CurrentStepID, isFirstTurn)
	// Restrict to the agent's enabled tools, then wrap survivors with the per-agent auth
	// gate + per-tool config delivery (enforced at execution).
	effectiveTools := gateTools(filterTools(d.tools, agentConfig), agentConfig, d.authRequiringTools, d.sessionAuth, session.ConversationID)
	var workflow *ConversationWorkflow
	if agentConfig != nil {
		workflow = agentConfig.Workflow
	}

	// 1. Immediate ack (202).
	sink(immediateResponse(requestID, 202, "Processing your request...", nil))

	// 2. Stream the turn in a goroutine, NOT inline. A turn that calls a
	//    confirmation-gated tool PARKS awaiting a later confirm_tool_action frame; the
	//    connection's read loop dispatches that frame, so running the turn inline would
	//    block the reader and deadlock (the confirm could never be read). Spawning frees
	//    the reader to receive the confirmation while the turn streams its events through
	//    the sink. Mirrors the Rust tokio::spawn / the Python ensure_future. The 202 ack
	//    above is already on the wire, and the terminal eventual_response is emitted from
	//    the goroutine on completion. The WaitGroup lets the connection loop await every
	//    in-flight turn on teardown (graceful drain).
	//
	//    The turn uses a context decoupled from the per-frame ctx: the read loop's
	//    Dispatch returns as soon as this goroutine is spawned, and the per-frame ctx
	//    (ioCtx) stays alive for the whole connection, so the turn keeps the connection's
	//    lifetime — torn down (and the gate unparked) only when the connection closes.
	d.turns.Add(1)
	go func() {
		defer d.turns.Done()
		runner := NewTurnRunner(d.client, d.store, effectiveSystemPrompt, d.knowledge, effectiveTools, d.confirmTools, d.confirmations, workflow, session.CurrentStepID, d.judgeModel)
		result, err := runner.Run(ctx, frame.SessionID, session.ConversationID, requestID, frame.Message, sink)
		if err != nil {
			// A turn failed (no engine configured, or a model/DB error). Emit a clean
			// error and keep the connection alive. Detail stays server-side.
			sink(errorEvent(requestID, "INTERNAL_ERROR", "Internal error processing the request."))
			return
		}
		// SMOODEV-590 — persist the workflow pointer the judge advanced to, so the next
		// turn on this conversation resumes on the right step. No-op for freeform agents
		// (NextStepID empty) or when unchanged. A persistence error must not fail the
		// already-produced turn, so it's swallowed (the step simply doesn't advance).
		if result.NextStepID != "" && result.NextStepID != session.CurrentStepID {
			_ = d.store.SetCurrentStep(ctx, frame.SessionID, result.NextStepID)
		}
		// 3. Terminal eventual_response.
		sink(eventualResponse(requestID, 200, result.MessageID, generalResponse(result.Reply), false, result.Citations))
	}()
}

// handleConfirmToolAction resumes a turn parked on a write-tool confirmation.
//
// Per spec/actions/confirm-tool-action.schema.json the client replies with
// {action, sessionId, requestId, approved} to a write_confirmation_required event.
// We resolve the session's pending confirmation with the verdict: the parked HumanGate
// returns and the turn resumes (runs the tool on approve, skips it with a rejection
// result on deny). There is no dedicated response event — continuation is signalled by
// the resumed streaming sequence; we ack with an immediate_response. Resolving takes the
// channel out, so a duplicate confirm is a clean NO_PENDING_CONFIRMATION no-op. Fails
// closed: a missing sessionId or non-bool approved is rejected (never silently approve).
func (d *FrameDispatcher) handleConfirmToolAction(frame inboundFrame, sink EventSink) {
	if frame.SessionID == "" {
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "confirm_tool_action requires a 'sessionId'"))
		return
	}
	if frame.Approved == nil {
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "confirm_tool_action requires a boolean 'approved'"))
		return
	}
	approved := *frame.Approved

	if !d.confirmations.Resolve(frame.SessionID, approved) {
		sink(errorEvent(frame.RequestID, "NO_PENDING_CONFIRMATION", "no tool action is awaiting confirmation for session '"+frame.SessionID+"'"))
		return
	}

	message := "Tool action rejected"
	if approved {
		message = "Tool action approved"
	}
	sink(immediateResponse(frame.RequestID, 200, message, map[string]any{
		"sessionId": frame.SessionID,
		"approved":  approved,
	}))
}
