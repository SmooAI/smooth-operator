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
// userName/userEmail are accepted for protocol parity but not retained by the
// in-memory reference store.
func (s *InMemorySessionStore) CreateSession(_ context.Context, agentID, _ /*userName*/, _ /*userEmail*/ string) (StoredSession, error) {
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
