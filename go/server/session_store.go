package server

import (
	"context"
	"sync"
	"time"

	"github.com/google/uuid"
)

// MessageDirection records who a stored message came from.
type MessageDirection int

const (
	// Inbound is a message from the user.
	Inbound MessageDirection = iota
	// Outbound is a message from the agent.
	Outbound
)

// StoredSession is a conversation session: the unit the protocol's create/get
// operate on. The Go analog of the C# StoredSession / Rust session record.
type StoredSession struct {
	SessionID          string
	ConversationID     string
	AgentID            string
	AgentName          string
	UserParticipantID  string
	AgentParticipantID string
	// CurrentStepID is the conversation's current step id within the agent's
	// conversation workflow. "" means "not started" → the first step is rendered.
	// Advanced by the post-turn workflow judge and persisted so the next turn resumes on
	// the right step. SMOODEV-590.
	CurrentStepID string
	// ContactEmail is the caller's email captured at create-session time, used as the OTP
	// delivery contact (the server offers OTP to it when an end_user tool is refused). The
	// reference create path captures only an email; a host that also captures a phone would
	// add an SMS channel. th-8078dd.
	ContactEmail string
	// OtpVerified is the session's identity-verified bit, set by a successful verify_otp and
	// threaded into the auth gate so a verified caller's end_user tools run. The Go analog of
	// the Rust session metadata.otpVerified. th-8078dd.
	OtpVerified bool
	// OwnerEmail is the AUTHENTICATED principal's email at create time — the conversation
	// ownership key every read is filtered by. Deliberately NOT ContactEmail: ContactEmail is
	// client-supplied (the OTP delivery address) and therefore spoofable, so it can never
	// decide who may read what. "" means the session was created with auth disabled, and is
	// visible only to an equally unscoped (auth-disabled) reader. th-8fe998.
	OwnerEmail string
}

// ConversationScope is the visibility filter for conversation reads: WHO is asking, derived
// from the connection's authenticated principal (AccessContext.ConversationScope) and never
// from client-supplied frame fields.
//
// The ZERO VALUE denies everything. That is deliberate: an implementer who forgets to
// populate it leaks nothing, and there is no way to spell "show me everything" by accident —
// Unscoped must be set explicitly, and only the no-auth path sets it. th-8fe998.
type ConversationScope struct {
	// Unscoped makes every conversation visible. Set ONLY when the server has no auth
	// configured (local/dev single-tenant). This is the one and only unscoped path.
	Unscoped bool
	// Email is the authenticated principal's email; a conversation is visible only when its
	// OwnerEmail matches exactly. Empty with Unscoped false ⇒ nothing is visible.
	Email string
}

// Allows reports whether a conversation owned by ownerEmail is visible in this scope. An
// empty Email never matches — including against an empty ownerEmail — so an auth-enabled
// connection whose principal carries no email cannot see the sessions created while auth was
// disabled. th-8fe998.
func (s ConversationScope) Allows(ownerEmail string) bool {
	if s.Unscoped {
		return true
	}
	return s.Email != "" && ownerEmail == s.Email
}

// StoredMessage is one persisted conversation message.
type StoredMessage struct {
	ID             string
	ConversationID string
	Direction      MessageDirection
	Text           string
	// CreatedAt is when the message was appended (UTC), the createdAt field of the
	// get_conversation_messages contract. Display only — paging keys off ID. th-669d48.
	CreatedAt time.Time
}

// ConversationSummary is one row of the conversation-list / resume surface: identity,
// last activity, message count, and the first inbound (user) message text — enough for
// the handler to build a sidebar title without a second store roundtrip. The Go analog of
// the Rust list_conversations' per-conversation peek; formatting (title truncation, ISO
// timestamp) is the handler's job. th-d5b446.
type ConversationSummary struct {
	ConversationID string
	UpdatedAt      time.Time
	MessageCount   int
	// FirstInbound is the first inbound (user) message text, "" when the conversation has
	// no inbound message (title falls back to a generic name).
	FirstInbound string
}

// SessionStore is persistence for sessions + conversation message logs — the Go
// analog of the C# ISessionStore and the Rust StorageAdapter's session/conversation/
// message surface. Async-shaped (context-taking) so a Postgres/Dynamo adapter can
// implement the same interface for durability; the bundled InMemorySessionStore is
// the reference store.
type SessionStore interface {
	// CreateSession mints a fresh session owned by scope's principal (Email; "" when auth is
	// disabled). userEmail stays the client-supplied OTP contact and MUST NOT be used for
	// ownership — it is attacker-controlled. th-8fe998.
	CreateSession(ctx context.Context, agentID, userName, userEmail string, scope ConversationScope) (StoredSession, error)
	// ResumeSession mints a session bound to an existing conversation when conversationID is
	// non-empty, known, AND visible to scope (returns resumed=true, reusing its message log
	// so subsequent turns append to it); an empty, unknown, or SOMEONE ELSE'S conversationID
	// mints a fresh conversation (resumed=false) — identical to CreateSession.
	//
	// Treating another user's conversation exactly like an unknown one is the security
	// contract, not an accident: it makes "not yours" and "never existed" indistinguishable to
	// the caller, so the resume path cannot be used as an oracle to enumerate other users'
	// conversation ids. Implementations MUST NOT report the difference. th-d5b446, th-8fe998.
	ResumeSession(ctx context.Context, agentID, userName, userEmail string, scope ConversationScope, conversationID string) (session StoredSession, resumed bool, err error)
	// ListConversations returns a summary per conversation that is visible to scope and has at
	// least one message (empty conversations — every page-load currently mints one — are
	// filtered out), in no particular order; the handler sorts most-recent-first and caps.
	//
	// The scope filter MUST be applied during selection, never to an already-truncated page:
	// filtering after a limit silently returns short or empty pages. The scope parameter is
	// REQUIRED — there is no unscoped default — so every implementer is forced to confront who
	// may see what. The zero value denies everything. th-d5b446, th-8fe998.
	ListConversations(ctx context.Context, scope ConversationScope) ([]ConversationSummary, error)
	// GetSession returns the session for sessionID regardless of ownership — it is the raw
	// lookup primitive. Callers serving a client request MUST check the returned session's
	// OwnerEmail against the connection's scope and treat a mismatch as not-found; the
	// dispatcher routes every such read through FrameDispatcher.scopedSession. th-8fe998.
	GetSession(ctx context.Context, sessionID string) (*StoredSession, error)
	AppendMessage(ctx context.Context, conversationID string, direction MessageDirection, text string) (StoredMessage, error)
	// ListMessages returns the most recent limit messages for a conversation, oldest first.
	ListMessages(ctx context.Context, conversationID string, limit int) ([]StoredMessage, error)
	// SetCurrentStep persists a session's conversation-workflow step id (advanced by the
	// post-turn judge), so the next turn resumes on the right step. A no-op for an unknown
	// session. SMOODEV-590.
	SetCurrentStep(ctx context.Context, sessionID, stepID string) error
	// SetSessionAuthenticated persists a session's OTP-verified bit (set by a successful
	// verify_otp), so subsequent turns' auth gates let a verified caller's end_user tools run.
	// A no-op for an unknown session. The Go analog of the Rust set_session_authenticated.
	// th-8078dd.
	SetSessionAuthenticated(ctx context.Context, sessionID string, verified bool) error
}

// InMemorySessionStore is an in-process SessionStore. The Go analog of the Rust
// in-memory adapter and the C# InMemorySessionStore. Safe for concurrent use.
type InMemorySessionStore struct {
	mu       sync.Mutex
	sessions map[string]StoredSession
	messages map[string][]StoredMessage
	// updatedAt tracks each conversation's last activity (creation, then every append), the
	// sort key + updatedAt field for ListConversations. th-d5b446.
	updatedAt map[string]time.Time
	// owner maps conversation id → owning principal email ("" = created with auth disabled).
	// Set once at conversation creation and never rewritten, so a resume cannot re-home
	// someone else's conversation onto the resumer. th-8fe998.
	owner map[string]string
}

// NewInMemorySessionStore returns an empty in-memory store.
func NewInMemorySessionStore() *InMemorySessionStore {
	return &InMemorySessionStore{
		sessions:  map[string]StoredSession{},
		messages:  map[string][]StoredMessage{},
		updatedAt: map[string]time.Time{},
		owner:     map[string]string{},
	}
}

// CreateSession mints a fresh session (and an empty message log for its conversation).
// userName is accepted for protocol parity but not retained; userEmail is retained as the
// OTP delivery contact (ContactEmail) so the end_user auth-gate flow can offer verification.
func (s *InMemorySessionStore) CreateSession(ctx context.Context, agentID, userName, userEmail string, scope ConversationScope) (StoredSession, error) {
	session, _, err := s.ResumeSession(ctx, agentID, userName, userEmail, scope, "")
	return session, err
}

// ResumeSession mints a session bound to an existing conversation (resumed=true) when
// conversationID is non-empty and known, else a fresh one (resumed=false). Only a fresh
// conversation gets an empty message log seeded — a resume keeps the existing log so
// subsequent turns append to it.
func (s *InMemorySessionStore) ResumeSession(_ context.Context, agentID, _ /*userName*/, userEmail string, scope ConversationScope, conversationID string) (StoredSession, bool, error) {
	if agentID == "" {
		agentID = uuid.NewString()
	}
	s.mu.Lock()
	defer s.mu.Unlock()

	resumed := false
	convID := conversationID
	if convID != "" {
		// Known AND visible to this scope. The two conditions collapse into one boolean on
		// purpose: an unknown conversation and another user's conversation take the exact same
		// branch below, so the caller cannot tell them apart and cannot probe for which ids
		// exist. th-8fe998.
		_, known := s.messages[convID]
		resumed = known && scope.Allows(s.owner[convID])
	}
	if !resumed {
		convID = uuid.NewString() // absent, unknown, or not ours → fresh conversation
	}

	session := StoredSession{
		SessionID:          uuid.NewString(),
		ConversationID:     convID,
		AgentID:            agentID,
		AgentName:          "smooth-agent",
		UserParticipantID:  uuid.NewString(),
		AgentParticipantID: uuid.NewString(),
		// Client-supplied, used only as the OTP delivery contact — never for ownership.
		ContactEmail: userEmail,
		// Ownership comes from the authenticated principal alone. On a resume the session
		// inherits the conversation's EXISTING owner rather than re-stamping it, so resuming can
		// never quietly transfer a conversation. th-8fe998.
		OwnerEmail: scope.Email,
	}
	if resumed {
		session.OwnerEmail = s.owner[convID]
	}
	s.sessions[session.SessionID] = session
	if !resumed {
		s.messages[convID] = nil
		s.updatedAt[convID] = time.Now()
		s.owner[convID] = scope.Email
	}
	return session, resumed, nil
}

// GetSession returns the session for sessionID, or (nil, nil) if unknown.
func (s *InMemorySessionStore) GetSession(_ context.Context, sessionID string) (*StoredSession, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if session, ok := s.sessions[sessionID]; ok {
		return &session, nil
	}
	return nil, nil
}

// AppendMessage appends a message to a conversation's log.
func (s *InMemorySessionStore) AppendMessage(_ context.Context, conversationID string, direction MessageDirection, text string) (StoredMessage, error) {
	message := StoredMessage{
		ID:             uuid.NewString(),
		ConversationID: conversationID,
		Direction:      direction,
		Text:           text,
		CreatedAt:      time.Now().UTC(),
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	s.messages[conversationID] = append(s.messages[conversationID], message)
	s.updatedAt[conversationID] = time.Now()
	return message, nil
}

// ListConversations returns a summary per non-empty conversation (unordered). Empty
// conversations are dropped so the caller's sidebar isn't buried in the blanks every
// page-load mints. Messages are stored oldest-first, so the first inbound scan yields the
// title source. th-d5b446.
func (s *InMemorySessionStore) ListConversations(_ context.Context, scope ConversationScope) ([]ConversationSummary, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]ConversationSummary, 0, len(s.messages))
	for convID, msgs := range s.messages {
		if len(msgs) == 0 {
			continue
		}
		// Ownership filter runs here, during selection — NOT on the handler's already-capped
		// page. Filtering after a limit would hand back short/empty pages while other users'
		// conversations silently consumed the quota. th-8fe998.
		if !scope.Allows(s.owner[convID]) {
			continue
		}
		firstInbound := ""
		for _, m := range msgs {
			if m.Direction == Inbound {
				firstInbound = m.Text
				break
			}
		}
		out = append(out, ConversationSummary{
			ConversationID: convID,
			UpdatedAt:      s.updatedAt[convID],
			MessageCount:   len(msgs),
			FirstInbound:   firstInbound,
		})
	}
	return out, nil
}

// ListMessages returns the most recent limit messages for a conversation, oldest first.
func (s *InMemorySessionStore) ListMessages(_ context.Context, conversationID string, limit int) ([]StoredMessage, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	all := s.messages[conversationID]
	if limit > 0 && len(all) > limit {
		all = all[len(all)-limit:]
	}
	// Return a copy so callers can't mutate the store's slice.
	out := make([]StoredMessage, len(all))
	copy(out, all)
	return out, nil
}

// SetCurrentStep persists a session's workflow step id. A no-op for an unknown session.
func (s *InMemorySessionStore) SetCurrentStep(_ context.Context, sessionID, stepID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if session, ok := s.sessions[sessionID]; ok {
		session.CurrentStepID = stepID
		s.sessions[sessionID] = session
	}
	return nil
}

// SetSessionAuthenticated persists a session's OTP-verified bit. A no-op for an unknown session.
func (s *InMemorySessionStore) SetSessionAuthenticated(_ context.Context, sessionID string, verified bool) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if session, ok := s.sessions[sessionID]; ok {
		session.OtpVerified = verified
		s.sessions[sessionID] = session
	}
	return nil
}
