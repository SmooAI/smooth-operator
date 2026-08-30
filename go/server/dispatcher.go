package server

import (
	"context"
	"encoding/json"
	"log/slog"
	"runtime/debug"
	"sort"
	"strings"
	"sync"
	"time"
	"unicode"

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
	// hooks are engine ToolHooks threaded into every turn the runner builds (nil →
	// none). Set by the server after construction, alongside tools.
	hooks []core.ToolHook
	// skills resolves send_message.skill to its markdown body (th-ebe27d / Rust #338).
	// Set by the server after construction, alongside hooks — the constructor signature
	// is long enough. Nil → the feature is off and any skill field is a clean
	// SKILL_NOT_FOUND, so a multi-tenant deploy never serves host skills by accident.
	skills SkillResolver
	// memoryProvider supplies the turn's durable-recall store, scoped to this connection's
	// access (th-ebe27d / Rust #330). Set by the server after construction, alongside skills.
	// nil → no auto-recall, the default.
	memoryProvider MemoryProvider
	// associate records this connection's backplane targets as they are learned (set by
	// the connection loop). Nil → session/agent targets are never routable.
	associate func(target Target)
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
	// otpService is the host OTP identity-verification seam (nil → no OTP offered; a refused
	// end_user tool stays refused, behavior unchanged). When installed, a turn that refuses an
	// end_user tool on a session with a contact triggers the OTP-offer flow, and verify_otp
	// validates codes through it. th-8078dd.
	otpService OtpService
	// modelCeiling is the active model's hard output ceiling (max_output_tokens) from
	// the gateway's /model/info, forwarded to every turn runner to clamp max_tokens.
	// nil → the raised default is unclamped (EPIC th-1cc9fa).
	modelCeiling *int
	// interactionKinds is the stateless catalog of Rich Interaction kinds hosted this
	// connection (choices, …). Threaded into every turn so the runner registers one
	// raise tool per kind. Defaults to DefaultInteractionKinds() in the constructor.
	interactionKinds *InteractionKinds
	// interactions is the per-connection session-keyed park/resume registry a
	// submit_interaction frame resolves (the Rich Interactions analog of confirmations).
	// Shared with the turn runner's raise tools. Created on demand in the constructor.
	interactions *InteractionRegistry
	// The client's declared render capabilities (`supports`) are NOT held here. They used
	// to be, in a per-connection sessionId→caps map, which lost them on every reconnect
	// (a reconnect resumes the conversation on a NEW session id) and never pruned. They
	// now live on the conversation in the store and arrive on StoredSession.Supports —
	// see capabilitySet. th-13df6d.

	// turns tracks in-flight spawned send_message turns so the connection loop can wait
	// for them to finish (and flush their eventual_response) on teardown — the
	// graceful-drain contract. send_message runs its turn as a goroutine (so the read
	// loop stays free to receive a confirm_tool_action while a turn is parked).
	turns sync.WaitGroup

	// turnMu guards current — the connection's single active turn. A send_message turn
	// runs on its own goroutine while the read loop keeps dispatching frames, so the
	// slot is written by the reader (on dispatch) and cleared by the turn goroutine
	// (on completion): both paths must hold the mutex.
	turnMu sync.Mutex
	// current is the connection's ONE in-flight agent turn, or nil when idle. A `cancel`
	// frame cancels its context (aborting the turn at its next context checkpoint) and a
	// second send_message while it is set is rejected with TURN_IN_PROGRESS rather than
	// run concurrently. The Go analog of the Rust reader loop's current_turn JoinHandle.
	current *activeTurn
}

// activeTurn is the connection's in-flight send_message turn: the requestId echoed on
// the `cancelled` event, and the cancel func for the turn's context. Cancelling that
// context is the Go analog of aborting the Rust turn's JoinHandle — the turn unwinds at
// its next context checkpoint (the runner's event loop, a ctx-aware tool, the model
// call) and emits no terminal event.
type activeTurn struct {
	requestID string
	cancel    context.CancelFunc
}

// NewFrameDispatcher binds a dispatcher to a connection's stores + access context. The
// knowledge retriever (default nil) and tools (default none) are threaded into every
// turn the runner builds. confirmTools + confirmations wire write-confirmation HITL;
// pass nil/empty + a registry to disable.
func NewFrameDispatcher(store SessionStore, client core.ChatClient, access AccessContext, systemPrompt string, knowledge core.Knowledge, tools []core.Tool, confirmTools []string, confirmations *ConfirmationRegistry, agentConfigs AgentConfigResolver, judgeModel string, authRequiringTools map[string]bool, sessionAuth SessionAuthenticator, otpService OtpService, modelCeiling *int) *FrameDispatcher {
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
		interactionKinds:   DefaultInteractionKinds(),
		interactions:       NewInteractionRegistry(),
		agentConfigs:       agentConfigs,
		judgeModel:         judgeModel,
		authRequiringTools: authRequiringTools,
		sessionAuth:        sessionAuth,
		otpService:         otpService,
		modelCeiling:       modelCeiling,
	}
}

// WaitForTurns blocks until every in-flight spawned send_message turn has completed.
// send_message runs its turn as a background goroutine (so the read loop stays free to
// receive a confirm_tool_action while a turn is parked); the connection loop calls this
// in its teardown so an in-flight turn finishes — and its eventual_response is flushed —
// before the writer stops and the backplane detach runs (the graceful-drain contract).
func (d *FrameDispatcher) WaitForTurns() { d.turns.Wait() }

// takeTurn removes and returns the connection's active turn (nil when idle), so the
// caller owns cancelling it exactly once.
func (d *FrameDispatcher) takeTurn() *activeTurn {
	d.turnMu.Lock()
	defer d.turnMu.Unlock()
	turn := d.current
	d.current = nil
	return turn
}

// CancelTurn aborts the connection's in-flight turn WITHOUT emitting a `cancelled`
// event — the disconnect path (the client is gone, there is nobody to tell). A cancelled
// turn stops at its next context checkpoint, discards its partial assistant message
// (never persisted) and emits no terminal event; the user's message stays persisted.
// A no-op when no turn is running. Idempotent.
//
// NOT called on graceful shutdown: a SIGTERM drain deliberately lets an in-flight turn
// finish within the pod termination window (see the connection loop's drain branch).
func (d *FrameDispatcher) CancelTurn() {
	if turn := d.takeTurn(); turn != nil {
		turn.cancel()
	}
}

// handleCancel serves the `cancel` action (the "Stop button"): abort the connection's
// in-flight turn and emit the terminal `cancelled` event echoing the cancelled turn's
// requestId (falling back to the cancel frame's own, per the schema). A cancel with no
// active turn is a harmless no-op that emits NOTHING and leaves the connection live.
func (d *FrameDispatcher) handleCancel(frame inboundFrame, sink EventSink) {
	turn := d.takeTurn()
	if turn == nil {
		return // nothing running → silent no-op
	}
	turn.cancel()
	echo := turn.requestID
	if echo == "" {
		echo = frame.RequestID
	}
	sink(cancelled(echo))
}

// inboundFrame is the minimal envelope shared by every client→server action.
type inboundFrame struct {
	Action    string `json:"action"`
	RequestID string `json:"requestId"`
	// create_conversation_session
	AgentID   string `json:"agentId"`
	UserName  string `json:"userName"`
	UserEmail string `json:"userEmail"`
	// create_conversation_session — the client's render capabilities for this conversation
	// (per spec/actions/create-conversation-session.schema.json). A per-kind list gating
	// the Rich Interactions the server may emit mid-turn; a kind whose capability is
	// missing degrades to its conversational fallback. Unknown values are kept
	// (forward-compat: a future kind may gate on them).
	//
	// A POINTER because omitted and `[]` mean different things and json.Unmarshal
	// collapses both to a nil []string: nil here ⇒ the key was absent, which INHERITS
	// the resumed conversation's stored set (a reconnect re-declares nothing); a
	// non-nil empty slice ⇒ an explicit "I render nothing", which replaces it. th-13df6d.
	Supports *[]string `json:"supports"`
	// create_conversation_session — optional: resume an existing conversation (bind the new
	// session to it) when known; absent/unknown → a fresh conversation (unchanged). th-d5b446.
	ConversationID string `json:"conversationId"`
	// list_conversations / get_conversation_messages — optional max rows returned (default 50).
	Limit int `json:"limit"`
	// get_conversation_messages — optional opaque pagination cursor from a prior response's
	// nextCursor (a message id today); return only messages older than the one it names.
	// th-669d48.
	Cursor string `json:"cursor"`
	// get_session / send_message / confirm_tool_action
	SessionID string `json:"sessionId"`
	Message   string `json:"message"`
	// send_message — optional named skill to apply to THIS turn (th-ebe27d / Rust #338).
	// The wire carries the NAME; the server resolves the body and composes it into the
	// system prompt, so the persisted user message stays exactly what the user typed.
	Skill string `json:"skill"`
	// send_message — optional multimodal/file attachments. Captured RAW and parsed
	// separately (fail-soft) in handleSendMessage: a malformed images/files array is
	// dropped without rejecting the turn, mirroring the Rust reference's
	// from_value(...).ok().unwrap_or_default(). Folding them into this struct directly
	// would make a malformed array fail the whole-frame unmarshal, which is NOT
	// fail-soft.
	Images json.RawMessage `json:"images"`
	Files  json.RawMessage `json:"files"`
	// confirm_tool_action — *bool so a missing verdict is distinguishable from
	// false (fail closed: a missing/garbled approved must NOT silently approve).
	Approved *bool `json:"approved"`
	// verify_otp — the one-time code the user entered.
	Code string `json:"code"`
	// submit_interaction — resume a parked Rich Interaction. InteractionID must echo the
	// interaction_required event's id (a stale card can't resolve a newer park); Kind, when
	// present, is cross-checked against the parked kind. Declined resumes with a decline;
	// otherwise Values (raw, kind-shaped) is routed to the kind's server-side validator.
	InteractionID string          `json:"interactionId"`
	Kind          string          `json:"kind"`
	Declined      bool            `json:"declined"`
	Values        json.RawMessage `json:"values"`
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
	case "list_conversations":
		d.handleListConversations(ctx, frame, sink)
	case "get_conversation_messages":
		d.handleGetConversationMessages(ctx, frame, sink)
	case "send_message":
		d.handleSendMessage(ctx, frame, sink)
	case "cancel":
		d.handleCancel(frame, sink)
	case "confirm_tool_action":
		d.handleConfirmToolAction(frame, sink)
	case "submit_interaction":
		d.handleSubmitInteraction(ctx, frame, sink)
	case "verify_otp":
		d.handleVerifyOtp(ctx, frame, sink)
	case "":
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "Missing 'action'"))
	default:
		sink(errorEvent(frame.RequestID, "UNSUPPORTED_ACTION", "Unsupported action '"+frame.Action+"'"))
	}
}

// internalError is the ONE place a failure becomes a protocol INTERNAL_ERROR — so it is
// also the one place it MUST be recorded. The wire message stays generic (no detail leaks
// to the client), but the underlying error goes to the host log at Error level. A server
// that fails every turn while logging nothing is undiagnosable — th-e7ef23, where the .NET
// sibling did exactly that and hid a 401 from the gateway for the whole bench run.
func (d *FrameDispatcher) internalError(sink EventSink, requestID, action string, err error) {
	slog.Error("INTERNAL_ERROR handling frame", "action", action, "requestId", requestID, "err", err)
	sink(errorEvent(requestID, "INTERNAL_ERROR", "Internal error processing the request."))
}

// scopedSession is the ONLY way a handler may turn a client-supplied sessionId into a
// session. It loads the session and then hides it unless the connection's authenticated
// principal owns it — returning (nil, nil), exactly what an unknown sessionId returns, so
// every caller emits the identical SESSION_NOT_FOUND and no caller can distinguish "not
// yours" from "never existed".
//
// Every sessionId-taking handler (get_session, get_conversation_messages, send_message,
// verify_otp) routes through here rather than calling store.GetSession directly: the check
// lives once, at the chokepoint, instead of being re-derived — and forgotten — per handler.
// th-8fe998.
func (d *FrameDispatcher) scopedSession(ctx context.Context, sessionID string) (*StoredSession, error) {
	session, err := d.store.GetSession(ctx, sessionID)
	if err != nil || session == nil {
		return nil, err
	}
	if !d.access.ConversationScope().Allows(session.OwnerEmail, session.OwnerOrg) {
		return nil, nil
	}
	d.associateSession(session)
	return session, nil
}

// associateSession points the session (and its agent) at this connection, so a publish
// to either target reaches this socket. Called from scopedSession — already the chokepoint
// every sessionId-bearing action routes through — and from session creation, so no handler
// can work with a session the backplane does not know about. No-op without a hook.
func (d *FrameDispatcher) associateSession(session *StoredSession) {
	if d.associate == nil {
		return
	}
	d.associate(Target{Kind: "session", ID: session.SessionID})
	if session.AgentID != "" {
		d.associate(Target{Kind: "agent", ID: session.AgentID})
	}
}

func (d *FrameDispatcher) handleCreateSession(ctx context.Context, frame inboundFrame, sink EventSink) {
	// agentId is REQUIRED by the Request schema and the generated client type is
	// non-optional, so absent-or-blank is a MALFORMED REQUEST, not an agentless session.
	// The old code fabricated a UUID and th-68897a's first pass silently stored NULL —
	// both skip the validation that belongs at this boundary. The column stays nullable
	// for rows written before this check; it is just no longer reachable from here.
	if strings.TrimSpace(frame.AgentID) == "" {
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "Missing 'agentId'"))
		return
	}

	// Resume when the caller passes a known conversationId (bind the new session to it so
	// subsequent turns append to its log and the runner replays its history); absent/unknown
	// → a fresh conversation (byte-for-byte unchanged). th-d5b446.
	//
	// Ownership is stamped from the CONNECTION's authenticated principal. frame.UserEmail is
	// still forwarded as the OTP delivery contact, but it is attacker-chosen and so has no say
	// in who owns — or may later read — this conversation. th-8fe998.
	session, _, err := d.store.ResumeSession(ctx, frame.AgentID, frame.UserName, frame.UserEmail, d.access.ConversationScope(), frame.ConversationID)
	if err != nil {
		d.internalError(sink, frame.RequestID, "create_conversation_session", err)
		return
	}
	// A freshly created session never passes through scopedSession, so associate here too.
	d.associateSession(&session)
	// Record the client's declared render capabilities on the CONVERSATION, so a later
	// turn offers a kind's rich card only when its capability is present (else the
	// fallback) — and so does a turn on the session the NEXT reconnect mints. A frame
	// that omits the key declares nothing and writes nothing: it inherits whatever the
	// conversation last declared (already on session.Supports from the resume). A frame
	// that DOES declare wins, including an explicit `[]` — that is the text-only opt-out,
	// and it must replace the stored set rather than leave it for the reconnect after it.
	//
	// Best-effort, like the workflow step pointer: a storage error is logged, not fatal.
	// This session already has its capabilities from the frame; the blast radius is a
	// LATER reconnect that omits `supports` degrading to text-only.
	if frame.Supports != nil {
		if err := d.store.SetConversationSupports(ctx, session.ConversationID, *frame.Supports); err != nil {
			slog.Warn("failed to persist declared client capabilities; a later reconnect that omits 'supports' will degrade to text-only",
				"conversationId", session.ConversationID, "error", err)
		}
	}
	data := map[string]any{
		"sessionId":          session.SessionID,
		"conversationId":     session.ConversationID,
		"agentName":          session.AgentName,
		"userParticipantId":  session.UserParticipantID,
		"agentParticipantId": session.AgentParticipantID,
	}
	// Omitted rather than "" when the session has no agent — absent is the honest wire
	// shape, and a fabricated id was the bug (th-68897a).
	if session.AgentID != "" {
		data["agentId"] = session.AgentID
	}
	sink(immediateResponse(frame.RequestID, 200, "Session created", data))
}

func (d *FrameDispatcher) handleGetSession(ctx context.Context, frame inboundFrame, sink EventSink) {
	session, err := d.scopedSession(ctx, frame.SessionID)
	if err != nil {
		d.internalError(sink, frame.RequestID, "get_session", err)
		return
	}
	if session == nil {
		sink(errorEvent(frame.RequestID, "SESSION_NOT_FOUND", "Session not found"))
		return
	}
	data := map[string]any{
		"sessionId":      session.SessionID,
		"conversationId": session.ConversationID,
		"agentName":      session.AgentName,
	}
	if session.AgentID != "" {
		data["agentId"] = session.AgentID
	}
	sink(immediateResponse(frame.RequestID, 200, "OK", data))
}

// defaultListLimit caps list_conversations when the caller doesn't ask for a specific limit.
const defaultListLimit = 50

// defaultConversationName is the title fallback for a conversation with messages but no
// inbound (user) message to preview. The Go store carries no per-conversation name (unlike
// the Rust reference's conversation.name), so a generic label stands in.
const defaultConversationName = "Conversation"

// handleListConversations — the conversation-sidebar / resume substrate. Returns the store's
// conversations that have at least one message (empty conversations, minted every page-load,
// are filtered out by the store), most-recent-first, each with a short title preview + message
// count. Reply is an immediate_response carrying { conversations: [ { conversationId, title,
// updatedAt, messageCount } ] }. Optional input: limit (default 50). Mirrors the Rust
// list_conversations. th-d5b446.
func (d *FrameDispatcher) handleListConversations(ctx context.Context, frame inboundFrame, sink EventSink) {
	limit := defaultListLimit
	if frame.Limit > 0 {
		limit = frame.Limit
	}

	// Scoped in the store's selection, before the sort+cap below — a filter applied to an
	// already-capped page would silently return short/empty pages. th-8fe998.
	summaries, err := d.store.ListConversations(ctx, d.access.ConversationScope())
	if err != nil {
		sink(errorEvent(frame.RequestID, "STORAGE_ERROR", "Failed to list conversations."))
		return
	}

	// Most-recent-first (stable so equal timestamps keep insertion order), then cap.
	sort.SliceStable(summaries, func(i, j int) bool {
		return summaries[i].UpdatedAt.After(summaries[j].UpdatedAt)
	})
	if len(summaries) > limit {
		summaries = summaries[:limit]
	}

	conversations := make([]map[string]any, 0, len(summaries))
	for _, c := range summaries {
		conversations = append(conversations, map[string]any{
			"conversationId": c.ConversationID,
			"title":          conversationTitle(c.FirstInbound, defaultConversationName),
			"updatedAt":      c.UpdatedAt.UTC().Format(time.RFC3339),
			"messageCount":   c.MessageCount,
		})
	}
	sink(immediateResponse(frame.RequestID, 200, "Conversations", map[string]any{"conversations": conversations}))
}

// maxMessageLimit caps get_conversation_messages' limit per the contract (1..100).
const maxMessageLimit = 100

// handleGetConversationMessages — the conversation-resume substrate. Returns a session's
// conversation messages NEWEST-first with nextCursor + hasMore, per
// spec/actions/get-messages.schema.json. Optional input: limit (1..100, default 50) and
// cursor (opaque — a message id; returns only messages older than the one it names).
// Mirrors the Rust handle_get_conversation_messages. th-669d48.
func (d *FrameDispatcher) handleGetConversationMessages(ctx context.Context, frame inboundFrame, sink EventSink) {
	if frame.SessionID == "" {
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "Missing 'sessionId'"))
		return
	}
	session, err := d.scopedSession(ctx, frame.SessionID)
	if err != nil {
		d.internalError(sink, frame.RequestID, "get_conversation_messages", err)
		return
	}
	if session == nil {
		sink(errorEvent(frame.RequestID, "SESSION_NOT_FOUND", "Session not found"))
		return
	}

	limit := defaultListLimit
	if frame.Limit > 0 {
		limit = frame.Limit
	}
	if limit > maxMessageLimit {
		limit = maxMessageLimit
	}

	// Without a cursor, limit+1 rows are enough — the extra one tells us whether more history
	// exists. With a cursor we need the whole log to locate the message it names.
	// ponytail: full-log read when paging; push the cursor into a store method if a
	// conversation ever gets big enough to care (would change SessionStore for implementers).
	fetch := limit + 1
	if frame.Cursor != "" {
		fetch = 0 // 0 = no cap
	}
	stored, err := d.store.ListMessages(ctx, session.ConversationID, fetch)
	if err != nil {
		sink(errorEvent(frame.RequestID, "STORAGE_ERROR", "Failed to list messages."))
		return
	}

	// Store returns oldest-first; the contract is newest-first.
	newestFirst := make([]StoredMessage, 0, len(stored))
	for i := len(stored) - 1; i >= 0; i-- {
		newestFirst = append(newestFirst, stored[i])
	}

	// The cursor names one exact message; the page starts immediately on its older side. An id
	// identifies a single row, so messages sharing a timestamp can neither be skipped nor
	// repeated — the failure mode a `created_at <` filter has by construction.
	if frame.Cursor != "" {
		at := -1
		for i, m := range newestFirst {
			if m.ID == frame.Cursor {
				at = i
				break
			}
		}
		if at < 0 {
			sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "Unknown 'cursor'"))
			return
		}
		newestFirst = newestFirst[at+1:]
	}

	hasMore := len(newestFirst) > limit
	if hasMore {
		newestFirst = newestFirst[:limit]
	}

	// nextCursor names the oldest message in this page — non-null exactly when hasMore.
	var nextCursor any
	if hasMore {
		nextCursor = newestFirst[len(newestFirst)-1].ID
	}

	messages := make([]map[string]any, 0, len(newestFirst))
	for _, m := range newestFirst {
		direction := "outbound"
		if m.Direction == Inbound {
			direction = "inbound"
		}
		messages = append(messages, map[string]any{
			"id":        m.ID,
			"direction": direction,
			"content":   map[string]any{"text": m.Text},
			// Display only — paging goes through the id cursor. Nano anyway: truncating to
			// whole seconds throws away ordering information clients render.
			"createdAt": m.CreatedAt.UTC().Format(time.RFC3339Nano),
		})
	}
	sink(immediateResponse(frame.RequestID, 200, "ConversationMessages", map[string]any{
		"messages":   messages,
		"nextCursor": nextCursor,
		"hasMore":    hasMore,
	}))
}

// conversationTitle derives a sidebar title: a trimmed, ~60-char preview of the first inbound
// message with leading markdown/control chars stripped, falling back to fallback when there's
// no inbound text. Mirrors the Rust conversation_title (plus the contract's leading-markdown
// strip). th-d5b446.
func conversationTitle(firstInbound, fallback string) string {
	t := stripLeadingMarkup(firstInbound)
	if t == "" {
		return fallback
	}
	return truncatePreview(t, 60)
}

// stripLeadingMarkup trims leading whitespace, control chars, and markdown syntax markers
// (heading #, bullets *-, quote >, emphasis _~, code `) off a preview so a message like
// "### Hi" or "- do X" titles as "Hi" / "do X".
func stripLeadingMarkup(s string) string {
	return strings.TrimLeftFunc(s, func(r rune) bool {
		return unicode.IsSpace(r) || unicode.IsControl(r) || strings.ContainsRune("#*>-_~`", r)
	})
}

// truncatePreview trims s and clips it to max runes (char-safe), appending an ellipsis when
// clipped. Mirrors the Rust truncate_preview.
func truncatePreview(s string, max int) string {
	s = strings.TrimSpace(s)
	r := []rune(s)
	if len(r) <= max {
		return s
	}
	return strings.TrimRight(string(r[:max]), " ") + "…"
}

func (d *FrameDispatcher) handleSendMessage(ctx context.Context, frame inboundFrame, sink EventSink) {
	// ONE active turn per connection: reject a second send_message while one is in
	// flight rather than run two concurrently (interleaved streams + racing storage
	// writes). The client must cancel the running turn or wait for it. Checked before
	// any other validation — and before the 202 ack — so the rejected frame produces
	// exactly one event. (confirm_tool_action / verify_otp RESUME the active turn, so
	// they are deliberately unaffected.)
	d.turnMu.Lock()
	busy := d.current != nil
	d.turnMu.Unlock()
	if busy {
		sink(errorEvent(frame.RequestID, "TURN_IN_PROGRESS",
			"a turn is already in progress on this connection; cancel it or wait for it to complete"))
		return
	}

	requestID := frame.RequestID
	if requestID == "" {
		requestID = uuid.NewString()
	}
	session, err := d.scopedSession(ctx, frame.SessionID)
	if err != nil {
		d.internalError(sink, requestID, "send_message", err)
		return
	}
	if session == nil {
		sink(errorEvent(requestID, "SESSION_NOT_FOUND", "Session not found"))
		return
	}

	// Resolve the optional `skill` BEFORE the ack: the turn either runs WITH the skill or
	// does not run at all. Resolving after the 202 would leave the client holding an
	// accepted turn that then errors, and an unknown skill would otherwise silently
	// degrade into an unskilled answer. A blank field is "no skill", not an unknown one.
	var skillSection string
	if name := strings.TrimSpace(frame.Skill); name != "" {
		section, ok := ResolveSkillSection(ctx, d.skills, name)
		if !ok {
			sink(errorEvent(requestID, "SKILL_NOT_FOUND",
				"skill '"+name+"' is not available on this server"))
			return
		}
		skillSection = section
	}

	// Resolve this agent's per-agent config (instructions, conversation workflow,
	// greeting, personality, tool allow-list) by the session's agent id, and fold it into
	// the effective system prompt + tools for THIS turn (SMOODEV-590). An un-configured
	// agent (no resolver / nil config) falls back to the server default prompt + full tool
	// set — behavior unchanged. Resolution never fails the turn: a resolver error degrades
	// to the default.
	var agentConfig *AgentConfig
	// An agentless session has nothing to resolve — don't ask the resolver about an
	// agent that does not exist (th-68897a).
	if d.agentConfigs != nil && session.AgentID != "" {
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
	// The invoked skill goes LAST: it is this turn's explicit instruction and should read
	// as the most recent, most specific directive the model sees.
	if skillSection != "" {
		effectiveSystemPrompt += "\n\n" + skillSection
	}
	// Thread the session's OTP-verified bit (from a prior successful verify_otp) into the
	// auth gate so a verified caller's end_user tools run — the Go analog of Rust threading
	// metadata.otpVerified into build_auth_gate. A verified session short-circuits to
	// authenticated; otherwise the host SessionAuthenticator seam (nil → fail-closed) decides.
	effectiveAuth := d.sessionAuth
	if session.OtpVerified {
		effectiveAuth = authenticatedSession{}
	}
	// Per-turn recorder: the auth gate writes the end_user tool it refused for lack of a
	// verified session, so after the turn we can offer OTP. th-8078dd.
	refusal := &otpRefusal{}
	// SEP extension hosting (th-829d9f): discover + spawn allowlisted extensions for THIS
	// turn and fold their tools into the base set BEFORE filtering, so an <ext>.<tool>
	// composes with the SMOODEV-590 enabled_tools / authLevel gate exactly like a built-in
	// tool. Default deny (empty SMOOTH_EXTENSIONS_ALLOW) → nil host, zero overhead, behavior
	// unchanged. The host is closed when the turn goroutine finishes (below).
	extTurn := buildExtensionHost(ctx, frame.SessionID, requestID, sink, d.confirmations)
	baseTools := d.tools
	if extTurn != nil {
		baseTools = append(append([]core.Tool{}, d.tools...), extTurn.Tools()...)
	}
	// Restrict to the agent's enabled tools, then wrap survivors with the per-agent auth
	// gate + per-tool config delivery (enforced at execution).
	effectiveTools := gateTools(filterTools(baseTools, agentConfig), agentConfig, d.authRequiringTools, effectiveAuth, session.ConversationID, refusal)
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
	// The turn's OWN cancellable context, derived from the connection's: a `cancel`
	// frame (or a disconnect) cancels it, unwinding the turn at its next context
	// checkpoint — the Go analog of aborting the Rust turn's JoinHandle. Registered as
	// the connection's single active turn before the goroutine starts, so a cancel that
	// lands immediately after this frame still finds it.
	turnCtx, turnCancel := context.WithCancel(ctx)
	// Per-turn host context: the turn's image + file attachments (surfaced to host
	// tools) and the directive sink (where a host tool writes a client-side directive
	// that lands on the terminal eventual_response). Parsed fail-soft — a malformed
	// images/files array is dropped, never rejecting the turn — mirroring the Rust
	// reference (handler.rs images parse + tool_provider.rs ToolProviderContext). The
	// engine dispatches every tool with turnCtx, so a host tool reads this via
	// TurnContextFrom(ctx). Empty attachments + no directive ⇒ behavior unchanged.
	turnState := &TurnContext{}
	// Drop a malformed array wholesale (Go's Unmarshal partially populates a slice on a
	// bad element, so discard the partial result) — matching the Rust reference's
	// from_value(...).ok().unwrap_or_default() all-or-nothing fail-soft.
	if len(frame.Images) > 0 {
		if err := json.Unmarshal(frame.Images, &turnState.Images); err != nil {
			turnState.Images = nil
		}
	}
	if len(frame.Files) > 0 {
		if err := json.Unmarshal(frame.Files, &turnState.Files); err != nil {
			turnState.Files = nil
		}
	}
	turnCtx = withTurnContext(turnCtx, turnState)
	turn := &activeTurn{requestID: requestID, cancel: turnCancel}
	d.turnMu.Lock()
	d.current = turn
	d.turnMu.Unlock()

	// EVERY frame this turn produces goes through this one gate, so `cancelled` is
	// genuinely terminal. A per-emit ctx check is not enough: the runner's event loop
	// checks ctx, but a Rich Interaction raise tool and the write-confirmation gate call
	// sink() straight from inside the tool, on the engine's goroutine, with no check at
	// all — so an interaction_required or a toolCall chunk could land AFTER the client
	// was told the turn was cancelled. Gating the sink itself is the choke point no emit
	// path can walk past, and mirrors the .NET dispatcher's turnSink.
	turnSink := func(event map[string]any) {
		if turnCtx.Err() != nil {
			return
		}
		sink(event)
	}

	d.turns.Add(1)
	go func() {
		defer d.turns.Done()
		// Always release the turn's context (a completed turn must not leak it), and
		// vacate the active-turn slot — but ONLY if it is still ours: a cancel already
		// took it out, and a later send_message may have installed its own.
		defer func() {
			d.turnMu.Lock()
			if d.current == turn {
				d.current = nil
			}
			d.turnMu.Unlock()
			turnCancel()
		}()
		// Tear the extension host down when the turn finishes: unpark any hung
		// ui/confirm and shut the subprocesses down. No-op when no host was built.
		// Uses the connection ctx, not the turn's, so a cancelled turn still cleans up.
		defer extTurn.Close(ctx)
		// Confine a panicking turn to THIS goroutine. Go's default is to take the whole
		// PROCESS down on an unrecovered panic in ANY goroutine, and a turn runs on a bare
		// one — so a host store/config/hook that explodes would drop every other live
		// connection on the pod. Rust confines a panicking turn to its JoinHandle; this is
		// the Go equivalent: the client gets a clean INTERNAL_ERROR and the connection
		// stays up. Registered LAST so it unwinds FIRST — before turnCancel (whose
		// cancelled ctx would suppress the event) and before turns.Done (which lets
		// teardown close the sink out from under us).
		//
		// This covers the turn goroutine's own stack. A panic inside a TOOL is still fatal:
		// the engine runs the tool loop on its own recover-less goroutine in
		// smooth-operator-core (core/agent.go RunStream), so that guard belongs there.
		defer func() {
			if r := recover(); r != nil {
				slog.Error("turn panicked", "requestId", requestID, "panic", r, "stack", string(debug.Stack()))
				turnSink(errorEvent(requestID, "INTERNAL_ERROR", "Internal error processing the request."))
			}
		}()
		runner := NewTurnRunner(d.client, d.store, effectiveSystemPrompt, d.knowledge, effectiveTools, d.confirmTools, d.confirmations, workflow, session.CurrentStepID, d.judgeModel, d.modelCeiling)
		runner.hooks = d.hooks
		// Durable auto-recall, scoped the way retrieval is: the host decides which memory this
		// caller recalls from. nil (no provider, or one that declines this access) leaves the turn
		// without auto-recall — byte-for-byte unchanged.
		if d.memoryProvider != nil {
			runner.memory = d.memoryProvider.MemoryForAccess(d.access)
		}
		// Rich Interactions: give the runner the hosted kinds, the park/resume registry,
		// and this session's declared capabilities, so it registers one raise tool per
		// kind (rich park when the capability is declared, else conversational fallback).
		runner.interactionKinds = d.interactionKinds
		runner.interactions = d.interactions
		runner.capabilities = capabilitySet(session.Supports)
		// Span attribution: the owning org (grouped by smooai.org_id on the turn span).
		runner.orgID = d.access.Principal.Org
		result, err := runner.Run(turnCtx, frame.SessionID, session.ConversationID, requestID, frame.Message, turnSink)
		// Cancelled turn: the `cancelled` event is the turn's ONE terminal event (emitted
		// by handleCancel), so emit nothing further here — no eventual_response, and no
		// INTERNAL_ERROR for the context cancellation that unwound the turn. The partial
		// assistant reply is discarded (the runner never persisted it); the user's message
		// stays persisted, per the protocol.
		if turnCtx.Err() != nil {
			return
		}
		if err != nil {
			// A turn failed (no engine configured, or a model/DB error). Emit a clean
			// error and keep the connection alive. Detail stays server-side.
			d.internalError(turnSink, requestID, "send_message", err)
			return
		}
		// SMOODEV-590 — persist the workflow pointer the judge advanced to, so the next
		// turn on this conversation resumes on the right step. No-op for freeform agents
		// (NextStepID empty) or when unchanged. A persistence error must not fail the
		// already-produced turn, so it's swallowed (the step simply doesn't advance).
		if result.NextStepID != "" && result.NextStepID != session.CurrentStepID {
			_ = d.store.SetCurrentStep(ctx, frame.SessionID, result.NextStepID)
		}
		// If the auth gate refused an end_user tool for lack of a verified session this turn,
		// and a host OtpService is installed and the session has a contact to reach, offer the
		// OTP flow (prompt → dispatch → ack) BEFORE the terminal response — mirroring the Rust
		// reference ordering. The reference server does not park/auto-resume; the client
		// verifies via verify_otp and re-sends its message once the session is authenticated.
		//
		// The turn's ctx, NOT the connection's: this branch sends a real verification
		// code to a real person, and the turnCtx.Err() check above happens before the
		// awaits in between — so it must be re-checked here, and the send itself has to
		// be cancellable. offerOtp re-checks immediately before dispatching.
		if tool := refusal.refusedTool(); tool != "" && d.otpService != nil {
			contact := OtpContact{Email: session.ContactEmail, Phone: session.ContactPhone}
			if !contact.IsEmpty() {
				d.offerOtp(turnCtx, session.SessionID, tool, contact, requestID, turnSink)
			}
		}
		// 3. Terminal eventual_response. Drain the turn's directive sink (a host tool may
		//    have written a send_file / navigation directive); absent ⇒ the field is
		//    omitted (back-compat). Mirrors the Rust runner draining directive_sink onto
		//    eventual_response (runner.rs / protocol.rs).
		directive, hasDirective := turnState.Directive()
		turnSink(eventualResponse(requestID, 200, result.MessageID, generalResponse(result.Reply), false, result.Citations, result.Usage, directive, hasDirective))
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

// capabilitySet turns a conversation's stored `supports` list into the set the turn
// runner gates rich cards on. Empty/nil ⇒ text-only: every kind degrades to its
// conversational fallback.
func capabilitySet(supports []string) map[string]bool {
	caps := make(map[string]bool, len(supports))
	for _, c := range supports {
		if c != "" {
			caps[c] = true
		}
	}
	return caps
}

// handleSubmitInteraction resumes a turn parked on a Rich Interaction.
//
// Per spec/actions/submit-interaction.schema.json the client replies with
// {action, sessionId, requestId, interactionId, kind?, values?, declined?} to an
// interaction_required event. Validation is SERVER-SIDE, routed to the parked kind's
// validator against the spec the raise carried:
//   - invalid → an interaction_invalid event with per-field errors; the turn STAYS
//     parked so the card can resubmit (mirrors otp_invalid — never a terminal error);
//   - valid → the parked raise resumes with the canonical values, and an
//     immediate_response acks;
//   - declined:true → the raise resumes with a declined payload.
//
// The interactionId must echo the event's, so a stale submit can never resolve a newer
// park; the pending record is taken only on resolution, so a duplicate submit is a clean
// NO_PENDING_INTERACTION no-op. The requestId is load-bearing (it echoes the originating
// interaction_required and keys the resumed stream), so require it.
func (d *FrameDispatcher) handleSubmitInteraction(ctx context.Context, frame inboundFrame, sink EventSink) {
	if frame.RequestID == "" {
		sink(errorEvent("", "VALIDATION_ERROR", "submit_interaction requires a 'requestId'"))
		return
	}
	if frame.SessionID == "" {
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "submit_interaction requires a 'sessionId'"))
		return
	}

	// Peek the pending interaction WITHOUT consuming the park — an invalid submit must
	// leave the turn parked for a resubmit. Scope the session first: an unreadable session
	// reports the identical NO_PENDING_INTERACTION an unknown id produces, so a submit can
	// never land in another user's parked turn.
	session, err := d.scopedSession(ctx, frame.SessionID)
	if err != nil {
		d.internalError(sink, frame.RequestID, "submit_interaction", err)
		return
	}
	var pending pendingInteraction
	var havePending bool
	if session != nil {
		pending, havePending = d.interactions.Pending(frame.SessionID)
	}
	if !havePending {
		sink(errorEvent(frame.RequestID, "NO_PENDING_INTERACTION", "no interaction is awaiting submission for session '"+frame.SessionID+"'"))
		return
	}

	// The submit must target THIS interaction instance (and, when it names a kind, the
	// right kind) — a stale card can never resolve a newer park.
	if frame.InteractionID != pending.InteractionID {
		sink(errorEvent(frame.RequestID, "INTERACTION_MISMATCH", "the submitted 'interactionId' does not match the pending interaction"))
		return
	}
	if frame.Kind != "" && frame.Kind != pending.Kind {
		sink(errorEvent(frame.RequestID, "INTERACTION_MISMATCH", "the pending interaction is '"+pending.Kind+"', not '"+frame.Kind+"'"))
		return
	}

	// Decline path: resume the raise with a declined payload.
	if frame.Declined {
		if d.interactions.Resolve(frame.SessionID, pending.InteractionID, InteractionOutcome{Status: outcomeDeclined}) {
			sink(immediateResponse(frame.RequestID, 200, "Interaction declined", map[string]any{
				"sessionId":     frame.SessionID,
				"interactionId": pending.InteractionID,
				"declined":      true,
			}))
		}
		return
	}

	// Values path: route to the parked kind's server-side validator.
	if len(frame.Values) == 0 {
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "submit_interaction requires 'values' (or 'declined': true)"))
		return
	}
	kind := d.interactionKinds.Get(pending.Kind)
	if kind == nil {
		// A parked kind the catalog no longer hosts (shouldn't happen).
		sink(errorEvent(frame.RequestID, "NO_PENDING_INTERACTION", "interaction kind '"+pending.Kind+"' is not hosted by this server"))
		return
	}
	var values any
	if err := json.Unmarshal(frame.Values, &values); err != nil {
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "submit_interaction 'values' is not valid JSON"))
		return
	}

	canonical, fieldErrs := kind.Validate(pending.Spec, values)
	if len(fieldErrs) > 0 {
		// Retryable: the turn stays parked; the client re-renders the card with the
		// per-field errors (never a terminal error event).
		sink(interactionInvalid(frame.RequestID, pending.InteractionID, pending.Kind, fieldErrs, "Some fields need attention."))
		return
	}
	if d.interactions.Resolve(frame.SessionID, pending.InteractionID, InteractionOutcome{Status: outcomeSubmitted, Values: canonical}) {
		// Kind-routed host effect (identity_intake: stamp the session identity). Kind-agnostic:
		// a kind with no host effect (choices) is a no-op. Runs on the valid rich-submit path;
		// the conversational fallback fires the same seam from the submit_interaction tool.
		attachInteractionEffect(ctx, d.store, kind, frame.SessionID, canonical)
		sink(immediateResponse(frame.RequestID, 200, "Interaction submitted", map[string]any{
			"sessionId":     frame.SessionID,
			"interactionId": pending.InteractionID,
		}))
	}
}

// offerOtp emits the OTP-offer sequence for a turn whose end_user tool was refused for lack
// of a verified session: otp_verification_required (prompt the client), then SendOtp on the
// host service, then otp_sent (ack delivery) — or an error event if delivery fails. The masked
// destination + channel come from the host; the server never sees the code. authLevel is fixed
// "end_user" (the only level this flow remedies). Mirrors the Rust offer_otp. th-8078dd.
func (d *FrameDispatcher) offerOtp(ctx context.Context, sessionID, tool string, contact OtpContact, requestID string, sink EventSink) {
	sink(otpVerificationRequired(
		requestID,
		tool,
		"Verify your identity to continue using '"+tool+"'.",
		contact.AvailableChannels(),
		"end_user",
	))
	// Adjacent to the side effect, not before the awaits that precede it: a cancel
	// landing here still means a real SMS/email to a real person, so this is the last
	// check before the code goes out.
	if ctx.Err() != nil {
		return
	}
	delivery, err := d.otpService.SendOtp(ctx, sessionID, contact)
	if err != nil {
		sink(errorEvent(requestID, "OTP_SEND_FAILED", "failed to send verification code"))
		return
	}
	sink(otpSent(requestID, delivery.Channel, delivery.MaskedDestination))
}

// handleVerifyOtp validates a submitted OTP code and, on success, marks the session
// identity-verified. Per spec/actions/verify-otp.schema.json the client sends
// {action, sessionId, requestId, code} in reply to an otp_verification_required event. There is
// no dedicated response event: a correct code emits otp_verified (the client then re-sends its
// message to run the gated tool — the reference server does not park/auto-resume the original
// turn), a rejected code emits otp_invalid carrying the host's remaining-attempt count. With no
// OtpService installed, verification is impossible, so we fail closed with an otp_invalid
// (NOT_FOUND, 0 attempts). Validation order mirrors the Rust reference:
// requestId → sessionId → code → session-exists → no-service. th-8078dd.
func (d *FrameDispatcher) handleVerifyOtp(ctx context.Context, frame inboundFrame, sink EventSink) {
	// requestId is load-bearing (it echoes the originating otp_verification_required); require it.
	if frame.RequestID == "" {
		sink(errorEvent("", "VALIDATION_ERROR", "verify_otp requires a 'requestId'"))
		return
	}
	if frame.SessionID == "" {
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "verify_otp requires a 'sessionId'"))
		return
	}
	if frame.Code == "" {
		sink(errorEvent(frame.RequestID, "VALIDATION_ERROR", "verify_otp requires a 'code'"))
		return
	}

	// The session must exist (a code can't verify a session we don't track).
	session, err := d.scopedSession(ctx, frame.SessionID)
	if err != nil {
		d.internalError(sink, frame.RequestID, "verify_otp", err)
		return
	}
	if session == nil {
		sink(errorEvent(frame.RequestID, "SESSION_NOT_FOUND", "session '"+frame.SessionID+"' not found"))
		return
	}

	// No host OTP service → verification is impossible. Fail closed on the documented
	// otp_invalid path (a client shouldn't reach here without first receiving
	// otp_verification_required, which only an installed service emits).
	if d.otpService == nil {
		sink(otpInvalid(frame.RequestID, OtpErrorNotFound, 0, "No verification is in progress for this session."))
		return
	}

	outcome := d.otpService.VerifyOtp(ctx, frame.SessionID, frame.Code)
	if outcome.OK {
		_ = d.store.SetSessionAuthenticated(ctx, frame.SessionID, true)
		sink(otpVerified(frame.RequestID, "Identity verified successfully."))
		return
	}
	sink(otpInvalid(frame.RequestID, outcome.Error, outcome.AttemptsRemaining, outcome.Message))
}
