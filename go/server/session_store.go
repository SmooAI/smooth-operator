package server

import (
	"context"
	"sync"

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
}

// SessionStore is persistence for sessions + conversation message logs — the Go
// analog of the C# ISessionStore and the Rust StorageAdapter's session/conversation/
// message surface. Async-shaped (context-taking) so a Postgres/Dynamo adapter can
// implement the same interface for durability; the bundled InMemorySessionStore is
// the reference store.
type SessionStore interface {
	CreateSession(ctx context.Context, agentID, userName, userEmail string) (StoredSession, error)
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
}

// NewInMemorySessionStore returns an empty in-memory store.
func NewInMemorySessionStore() *InMemorySessionStore {
	return &InMemorySessionStore{
		sessions: map[string]StoredSession{},
		messages: map[string][]StoredMessage{},
	}
}

// CreateSession mints a fresh session (and an empty message log for its conversation).
// userName is accepted for protocol parity but not retained; userEmail is retained as the
// OTP delivery contact (ContactEmail) so the end_user auth-gate flow can offer verification.
func (s *InMemorySessionStore) CreateSession(_ context.Context, agentID, _ /*userName*/, userEmail string) (StoredSession, error) {
	if agentID == "" {
		agentID = uuid.NewString()
	}
	session := StoredSession{
		SessionID:          uuid.NewString(),
		ConversationID:     uuid.NewString(),
		AgentID:            agentID,
		AgentName:          "smooth-agent",
		UserParticipantID:  uuid.NewString(),
		AgentParticipantID: uuid.NewString(),
		ContactEmail:       userEmail,
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	s.sessions[session.SessionID] = session
	s.messages[session.ConversationID] = nil
	return session, nil
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
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	s.messages[conversationID] = append(s.messages[conversationID], message)
	return message, nil
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
