package server

import (
	"context"
	"encoding/json"

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
	tools   []core.Tool
}

// NewFrameDispatcher binds a dispatcher to a connection's stores + access context. The
// tools (default none) are threaded into every turn the runner builds.
func NewFrameDispatcher(store SessionStore, client core.ChatClient, access AccessContext, systemPrompt string, tools []core.Tool) *FrameDispatcher {
	return &FrameDispatcher{store: store, client: client, access: access, systemP: systemPrompt, tools: tools}
}

// inboundFrame is the minimal envelope shared by every client→server action.
type inboundFrame struct {
	Action    string `json:"action"`
	RequestID string `json:"requestId"`
	// create_conversation_session
	AgentID   string `json:"agentId"`
	UserName  string `json:"userName"`
	UserEmail string `json:"userEmail"`
	// get_session / send_message
	SessionID string `json:"sessionId"`
	Message   string `json:"message"`
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

	// 1. Immediate ack (202).
	sink(immediateResponse(requestID, 202, "Processing your request...", nil))

	// 2. Stream the turn.
	runner := NewTurnRunner(d.client, d.store, d.systemP, d.tools)
	result, err := runner.Run(ctx, session.ConversationID, requestID, frame.Message, sink)
	if err != nil {
		// A turn failed (no engine configured, or a model/DB error). Emit a clean
		// error and keep the connection alive. Detail stays server-side.
		sink(errorEvent(requestID, "INTERNAL_ERROR", "Internal error processing the request."))
		return
	}

	// 3. Terminal eventual_response.
	sink(eventualResponse(requestID, 200, result.MessageID, generalResponse(result.Reply), false, result.Citations))
}
