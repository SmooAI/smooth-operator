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
	CreateSession(ctx context.Context, agentID, userName, userEmail string) (StoredSession, error)
	// ResumeSession mints a session bound to an existing conversation when conversationID is
	// non-empty AND known (returns resumed=true, reusing its message log so subsequent turns
	// append to it); an empty or unknown conversationID mints a fresh conversation
	// (resumed=false) — identical to CreateSession. The resume substrate behind
	// create_conversation_session's optional conversationId. th-d5b446.
	ResumeSession(ctx context.Context, agentID, userName, userEmail, conversationID string) (session StoredSession, resumed bool, err error)
	// ListConversations returns a summary per conversation that has at least one message
	// (empty conversations — every page-load currently mints one — are filtered out), in no
	// particular order; the handler sorts most-recent-first and caps. The Go analog of the
	// Rust storage.list_conversations_by_org + per-conversation peek. th-d5b446.
	ListConversations(ctx context.Context) ([]ConversationSummary, error)
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
}

// NewInMemorySessionStore returns an empty in-memory store.
func NewInMemorySessionStore() *InMemorySessionStore {
	return &InMemorySessionStore{
		sessions:  map[string]StoredSession{},
		messages:  map[string][]StoredMessage{},
		updatedAt: map[string]time.Time{},
	}
}

// CreateSession mints a fresh session (and an empty message log for its conversation).
// userName is accepted for protocol parity but not retained; userEmail is retained as the
// OTP delivery contact (ContactEmail) so the end_user auth-gate flow can offer verification.
func (s *InMemorySessionStore) CreateSession(ctx context.Context, agentID, userName, userEmail string) (StoredSession, error) {
	session, _, err := s.ResumeSession(ctx, agentID, userName, userEmail, "")
	return session, err
}

// ResumeSession mints a session bound to an existing conversation (resumed=true) when
// conversationID is non-empty and known, else a fresh one (resumed=false). Only a fresh
// conversation gets an empty message log seeded — a resume keeps the existing log so
// subsequent turns append to it.
func (s *InMemorySessionStore) ResumeSession(_ context.Context, agentID, _ /*userName*/, userEmail, conversationID string) (StoredSession, bool, error) {
	if agentID == "" {
		agentID = uuid.NewString()
	}
	s.mu.Lock()
	defer s.mu.Unlock()

	resumed := false
	convID := conversationID
	if convID != "" {
		_, resumed = s.messages[convID] // known conversation → bind to it
	}
	if !resumed {
		convID = uuid.NewString() // absent or unknown → fresh conversation
	}

	session := StoredSession{
		SessionID:          uuid.NewString(),
		ConversationID:     convID,
		AgentID:            agentID,
		AgentName:          "smooth-agent",
		UserParticipantID:  uuid.NewString(),
		AgentParticipantID: uuid.NewString(),
		ContactEmail:       userEmail,
	}
	s.sessions[session.SessionID] = session
	if !resumed {
		s.messages[convID] = nil
		s.updatedAt[convID] = time.Now()
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
func (s *InMemorySessionStore) ListConversations(_ context.Context) ([]ConversationSummary, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]ConversationSummary, 0, len(s.messages))
	for convID, msgs := range s.messages {
		if len(msgs) == 0 {
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
