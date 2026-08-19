package server

// Durable Postgres storage for the Go server — the swap behind the `ponytail:`
// seams in session_store.go and admin.go. One type, PostgresStore, implements
// BOTH the SessionStore (sessions / conversations / participants / messages) and
// the adminStore (connector configs, agent settings, indexing runs), because both
// live in the same database and neither is useful without the other.
//
// Selected by SMOOTH_AGENT_STORAGE=postgres (see StorageOptionsFromEnv); with the
// variable unset the server keeps its in-memory stores, unchanged.
//
// SCHEMA: copied from the Rust reference adapter
// (rust/adapters/postgres/src/schema.rs) so every server in this repo shares ONE
// set of tables — same names, same columns, no per-language dialect. Every
// statement is CREATE ... IF NOT EXISTS, so whichever server boots first creates
// the tables and the rest no-op.
//
// Ownership follows the Rust model rather than inventing a column: a
// conversation's owner is the email on its `user` participant row, which is
// exactly what the Rust adapter's list_conversations_by_org_and_user filters on.
// The per-session bits Go carries that Rust keeps in session metadata
// (contactEmail, otpVerified, currentStepId) live in conversation_sessions.metadata.

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
	"strings"
	"time"

	"github.com/google/uuid"
	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

// postgresSchema is the DDL applied on connect. The OLTP + admin tables are
// verbatim from rust/adapters/postgres/src/schema.rs; the one addition is the
// idempotent `org_id` column on indexing_runs, which the Rust IndexingStore does
// not scope by but the /admin/* API must (a run is listed per org). Adding it with
// ADD COLUMN IF NOT EXISTS mirrors how the Rust schema back-fills
// knowledge_vectors.acl, and leaves Rust's own queries untouched.
const postgresSchema = `
CREATE TABLE IF NOT EXISTS conversations (
    id              TEXT PRIMARY KEY,
    platform        TEXT NOT NULL
                        CHECK (platform IN ('web', 'messenger', 'instagram', 'email', 'discord',
                        'phone', 'sms', 'slack', 'whatsapp', 'tiktok')),
    name            TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    metadata_json   JSONB NOT NULL DEFAULT '{}'::jsonb,
    analytics_json  JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_conversations_org_idem
    ON conversations (organization_id, idempotency_key);
CREATE INDEX IF NOT EXISTS idx_conversations_org_created
    ON conversations (organization_id, created_at DESC);

CREATE TABLE IF NOT EXISTS conversation_participants (
    id                  TEXT PRIMARY KEY,
    conversation_id     TEXT NOT NULL,
    organization_id     TEXT NOT NULL,
    type                TEXT NOT NULL CHECK (type IN ('user', 'ai-agent', 'human-agent')),
    external_id         TEXT,
    internal_id         TEXT,
    browser_fingerprint TEXT,
    browser_info        JSONB,
    name                TEXT NOT NULL,
    email               TEXT,
    phone               TEXT,
    crm_contact_id      TEXT,
    metadata_json       JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_participants_conversation
    ON conversation_participants (conversation_id, created_at);
CREATE INDEX IF NOT EXISTS idx_participants_external
    ON conversation_participants (conversation_id, external_id);

CREATE TABLE IF NOT EXISTS conversation_messages (
    id              TEXT PRIMARY KEY,
    external_id     TEXT,
    organization_id TEXT,
    conversation_id TEXT,
    direction       TEXT NOT NULL CHECK (direction IN ('inbound', 'outbound')),
    content         JSONB NOT NULL,
    from_ref        JSONB,
    to_ref          JSONB,
    metadata_json   JSONB NOT NULL DEFAULT '{}'::jsonb,
    analytics_json  JSONB NOT NULL DEFAULT '{}'::jsonb,
    seq             BIGSERIAL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation_seq
    ON conversation_messages (conversation_id, seq);

CREATE TABLE IF NOT EXISTS conversation_sessions (
    session_id           TEXT PRIMARY KEY,
    conversation_id      TEXT NOT NULL,
    organization_id      TEXT NOT NULL DEFAULT '',
    -- Nullable: a session with no caller-supplied agent has NO agent (th-68897a).
    agent_id             TEXT,
    agent_name           TEXT NOT NULL,
    user_participant_id  TEXT NOT NULL,
    agent_participant_id TEXT NOT NULL,
    thread_id            TEXT NOT NULL,
    -- NULL passes a CHECK, so this constrains the value without making status required.
    status               TEXT CHECK (status IN ('active', 'idle', 'ended')),
    token_count          BIGINT,
    message_count        BIGINT,
    metadata             JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at             TIMESTAMPTZ,
    last_activity_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_sessions_conversation
    ON conversation_sessions (conversation_id, created_at);
CREATE INDEX IF NOT EXISTS idx_sessions_organization
    ON conversation_sessions (organization_id, created_at);

CREATE TABLE IF NOT EXISTS connector_configs (
    org_id     TEXT NOT NULL,
    id         TEXT NOT NULL,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    config     JSONB NOT NULL,
    enabled    BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, id)
);

CREATE TABLE IF NOT EXISTS agent_settings (
    org_id        TEXT PRIMARY KEY,
    model         TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    default_tools JSONB NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS indexing_runs (
    id               TEXT PRIMARY KEY,
    connector_name   TEXT NOT NULL,
    status           TEXT NOT NULL,
    started_at       TIMESTAMPTZ NOT NULL,
    finished_at      TIMESTAMPTZ,
    documents_seen   BIGINT NOT NULL,
    chunks_indexed   BIGINT NOT NULL,
    documents_skipped BIGINT NOT NULL,
    cursor           TIMESTAMPTZ,
    error            TEXT
);
CREATE INDEX IF NOT EXISTS idx_indexing_runs_connector_started
    ON indexing_runs (connector_name, started_at DESC);
-- Org scope for the /admin/* run list. Idempotent so a database whose
-- indexing_runs was created by the Rust adapter gains the column in place.
ALTER TABLE indexing_runs ADD COLUMN IF NOT EXISTS org_id TEXT;
CREATE INDEX IF NOT EXISTS idx_indexing_runs_org_started
    ON indexing_runs (org_id, started_at DESC);
`

// PostgresStore is the durable SessionStore + adminStore. Safe for concurrent use
// (pgxpool is).
type PostgresStore struct {
	pool *pgxpool.Pool
}

// NewPostgresStore connects to Postgres and applies the schema (idempotent).
// connString is a libpq URL or key=value DSN.
func NewPostgresStore(ctx context.Context, connString string) (*PostgresStore, error) {
	pool, err := pgxpool.New(ctx, connString)
	if err != nil {
		return nil, fmt.Errorf("postgres: connect: %w", err)
	}
	// Exec with no arguments goes out on the simple protocol, which is what lets the
	// whole multi-statement schema go in one round trip.
	if _, err := pool.Exec(ctx, postgresSchema); err != nil {
		pool.Close()
		return nil, fmt.Errorf("postgres: apply schema: %w", err)
	}
	return &PostgresStore{pool: pool}, nil
}

// Close releases the connection pool.
func (s *PostgresStore) Close() { s.pool.Close() }

// StorageOptionsFromEnv returns the Server options selecting the storage backend
// named by SMOOTH_AGENT_STORAGE — the same contract the Rust server uses:
//
//	memory (or unset) → no options; the in-memory stores stay in place
//	postgres          → durable session + admin stores on SMOOTH_AGENT_DATABASE_URL
//	                    (falling back to DATABASE_URL)
//
// Any other value is an error rather than a silent fallback to memory: a host that
// asked for durability and quietly got none is the failure mode worth shouting about.
func StorageOptionsFromEnv(ctx context.Context) ([]Option, error) {
	switch backend := os.Getenv("SMOOTH_AGENT_STORAGE"); backend {
	case "", "memory":
		return nil, nil
	case "postgres":
		conn := os.Getenv("SMOOTH_AGENT_DATABASE_URL")
		if conn == "" {
			conn = os.Getenv("DATABASE_URL")
		}
		if conn == "" {
			return nil, errors.New("SMOOTH_AGENT_STORAGE=postgres but neither SMOOTH_AGENT_DATABASE_URL nor DATABASE_URL is set")
		}
		store, err := NewPostgresStore(ctx, conn)
		if err != nil {
			return nil, err
		}
		return []Option{WithSessionStore(store), withAdminStore(store)}, nil
	default:
		return nil, fmt.Errorf("unknown SMOOTH_AGENT_STORAGE %q (want memory or postgres)", backend)
	}
}

// ── session metadata ────────────────────────────────────────────────────────

// sessionMetadata is the JSON held in conversation_sessions.metadata: the
// per-session bits Go's StoredSession carries that have no dedicated column in the
// shared schema. Mirrors the Rust reference server's session metadata (which is
// where its otpVerified lives too).
type sessionMetadata struct {
	UserName      string `json:"userName,omitempty"`
	ContactEmail  string `json:"contactEmail,omitempty"`
	ContactPhone  string `json:"contactPhone,omitempty"`
	OtpVerified   bool   `json:"otpVerified,omitempty"`
	CurrentStepID string `json:"currentStepId,omitempty"`
}

// messageContent is the stored shape of conversation_messages.content — the same
// {items, text} the Rust MessageContent serializes to, so a row written here reads
// back correctly in every other server.
type messageContent struct {
	Items []messageContentItem `json:"items"`
	Text  string               `json:"text"`
}

type messageContentItem struct {
	Type string `json:"type"`
	Text string `json:"text"`
}

// ── SessionStore ────────────────────────────────────────────────────────────

// CreateSession mints a fresh session, exactly as ResumeSession with no conversation id.
func (s *PostgresStore) CreateSession(ctx context.Context, agentID, userName, userEmail string, scope ConversationScope) (StoredSession, error) {
	session, _, err := s.ResumeSession(ctx, agentID, userName, userEmail, scope, "")
	return session, err
}

// ResumeSession binds to conversationID when it is known, in this org, AND visible to
// scope; anything else mints a fresh conversation. The rejection reasons (absent /
// unknown / someone else's / another org's) take the identical branch, so a caller
// cannot use resume as an oracle for which conversation ids exist. th-8fe998.
func (s *PostgresStore) ResumeSession(ctx context.Context, agentID, userName, userEmail string, scope ConversationScope, conversationID string) (StoredSession, bool, error) {
	// No agentId from the caller means NO agent, not a new one. This used to mint a fresh
	// UUID, which pointed every agentless session at an agent that has never existed —
	// invisible until something tried to resolve it (th-68897a). Blank/whitespace reads as
	// absent; "" is this struct's absent, so nothing downstream needs a pointer.
	agentID = strings.TrimSpace(agentID)

	resumed := false
	owner := scope.Email
	if conversationID != "" {
		existingOwner, found, err := s.conversationOwner(ctx, conversationID, scope.OrgID)
		if err != nil {
			return StoredSession{}, false, err
		}
		// Known AND visible collapse into ONE boolean on purpose — see the doc comment.
		// conversationOwner already filtered by scope.OrgID, so the org half is
		// satisfied by construction — pass it through rather than re-deriving it.
		if found && scope.Allows(existingOwner, scope.OrgID) {
			resumed, owner = true, existingOwner
		}
	}

	convID := conversationID
	if !resumed {
		convID = uuid.NewString()
		owner = scope.Email
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
		OwnerEmail:   owner,
		// A resumed conversation is in scope.OrgID by construction — conversationOwner
		// only matches rows whose organization_id equals it — so both branches stamp
		// the same org.
		OwnerOrg: scope.OrgID,
	}

	metadata, err := json.Marshal(sessionMetadata{ContactEmail: userEmail})
	if err != nil {
		return StoredSession{}, false, fmt.Errorf("postgres: encode session metadata: %w", err)
	}

	now := time.Now().UTC()
	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return StoredSession{}, false, fmt.Errorf("postgres: begin: %w", err)
	}
	defer func() { _ = tx.Rollback(ctx) }()

	if !resumed {
		// Fresh conversation. idempotency_key is the conversation id: unique per
		// conversation, which is what the (organization_id, idempotency_key) unique
		// index wants, and it never collides across orgs.
		if _, err := tx.Exec(ctx,
			`INSERT INTO conversations (id, platform, name, organization_id, idempotency_key, created_at, updated_at)
			 VALUES ($1, 'web', '', $2, $1, $3, $3)`,
			convID, scope.OrgID, now); err != nil {
			return StoredSession{}, false, fmt.Errorf("postgres: insert conversation: %w", err)
		}
		// The `user` participant carries the owner email — the same column the Rust
		// adapter reads ownership from. Written ONCE, when the conversation is minted;
		// a resume never adds a second user participant, so resuming can never
		// re-home someone else's conversation (nor claim an ownerless one).
		name := userName
		if name == "" {
			name = "user"
		}
		if _, err := tx.Exec(ctx,
			`INSERT INTO conversation_participants (id, conversation_id, organization_id, type, name, email, created_at, updated_at)
			 VALUES ($1, $2, $3, 'user', $4, $5, $6, $6)`,
			session.UserParticipantID, convID, scope.OrgID, name, nullIfBlank(scope.Email), now); err != nil {
			return StoredSession{}, false, fmt.Errorf("postgres: insert user participant: %w", err)
		}
		if _, err := tx.Exec(ctx,
			`INSERT INTO conversation_participants (id, conversation_id, organization_id, type, name, created_at, updated_at)
			 VALUES ($1, $2, $3, 'ai-agent', 'smooth-agent', $4, $4)`,
			session.AgentParticipantID, convID, scope.OrgID, now); err != nil {
			return StoredSession{}, false, fmt.Errorf("postgres: insert agent participant: %w", err)
		}
	}

	if _, err := tx.Exec(ctx,
		`INSERT INTO conversation_sessions
		    (session_id, conversation_id, organization_id, agent_id, agent_name, user_participant_id,
		     agent_participant_id, thread_id, status, metadata, created_at, updated_at, last_activity_at)
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $2, 'active', $8::jsonb, $9, $9, $9)`,
		session.SessionID, convID, scope.OrgID, nullIfBlank(agentID), session.AgentName,
		session.UserParticipantID, session.AgentParticipantID, string(metadata), now); err != nil {
		return StoredSession{}, false, fmt.Errorf("postgres: insert session: %w", err)
	}

	if err := tx.Commit(ctx); err != nil {
		return StoredSession{}, false, fmt.Errorf("postgres: commit: %w", err)
	}
	return session, resumed, nil
}

// conversationOwner returns the owner email of an org's conversation ("" when
// ownerless) and whether the conversation exists in that org at all. A conversation
// in ANOTHER org reports found=false — indistinguishable from never having existed.
func (s *PostgresStore) conversationOwner(ctx context.Context, conversationID, orgID string) (string, bool, error) {
	var owner string
	err := s.pool.QueryRow(ctx,
		`SELECT coalesce((SELECT p.email FROM conversation_participants p
		                   WHERE p.conversation_id = c.id AND p.type = 'user'
		                   ORDER BY p.created_at, p.id LIMIT 1), '')
		   FROM conversations c
		  WHERE c.id = $1 AND c.organization_id = $2`,
		conversationID, orgID).Scan(&owner)
	switch {
	case err == nil:
		return owner, true, nil
	case errors.Is(err, pgx.ErrNoRows):
		return "", false, nil
	default:
		return "", false, fmt.Errorf("postgres: resolve conversation owner: %w", err)
	}
}

// GetSession returns the session for sessionID, or (nil, nil) if unknown. The raw
// lookup primitive: ownership is REPORTED (OwnerEmail + OwnerOrg) but not enforced
// here — the dispatcher's scopedSession is the gate, and it applies org before owner.
// OwnerEmail is read from the conversation's user participant rather than duplicated
// onto the session row, so there is one source of truth and a resumed session reports
// the ORIGINAL owner.
//
// OwnerOrg MUST be populated for the gate to do its job. It reads the org off the
// session row and hands it to ConversationScope.Allows, which treats an EMPTY org as
// "unrecorded" and falls through to an ownership-only check. So a store that returns
// "" here does not fail loudly — it silently reopens the cross-org hole for every
// ownerless conversation, on the one backend that actually holds several orgs' data.
func (s *PostgresStore) GetSession(ctx context.Context, sessionID string) (*StoredSession, error) {
	var session StoredSession
	var metadata []byte
	err := s.pool.QueryRow(ctx,
		`SELECT s.conversation_id, coalesce(s.agent_id, ''), s.agent_name, s.user_participant_id, s.agent_participant_id,
		        s.organization_id,
		        coalesce(s.metadata, '{}'::jsonb),
		        coalesce((SELECT p.email FROM conversation_participants p
		                   WHERE p.conversation_id = s.conversation_id AND p.type = 'user'
		                   ORDER BY p.created_at, p.id LIMIT 1), '')
		   FROM conversation_sessions s
		  WHERE s.session_id = $1`,
		sessionID).Scan(&session.ConversationID, &session.AgentID, &session.AgentName,
		&session.UserParticipantID, &session.AgentParticipantID, &session.OwnerOrg,
		&metadata, &session.OwnerEmail)
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("postgres: get session: %w", err)
	}
	var meta sessionMetadata
	if err := json.Unmarshal(metadata, &meta); err != nil {
		return nil, fmt.Errorf("postgres: decode session metadata: %w", err)
	}
	session.SessionID = sessionID
	session.UserName = meta.UserName
	session.ContactEmail = meta.ContactEmail
	session.ContactPhone = meta.ContactPhone
	session.OtpVerified = meta.OtpVerified
	session.CurrentStepID = meta.CurrentStepID
	return &session, nil
}

// AppendMessage appends a message and bumps the conversation's last-activity time
// (the ListConversations sort key), in one transaction so the two never disagree.
func (s *PostgresStore) AppendMessage(ctx context.Context, conversationID string, direction MessageDirection, text string) (StoredMessage, error) {
	content, err := json.Marshal(messageContent{
		Items: []messageContentItem{{Type: "text", Text: text}},
		Text:  text,
	})
	if err != nil {
		return StoredMessage{}, fmt.Errorf("postgres: encode message content: %w", err)
	}

	message := StoredMessage{
		ID:             uuid.NewString(),
		ConversationID: conversationID,
		Direction:      direction,
		Text:           text,
		CreatedAt:      time.Now().UTC(),
	}

	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return StoredMessage{}, fmt.Errorf("postgres: begin: %w", err)
	}
	defer func() { _ = tx.Rollback(ctx) }()

	if _, err := tx.Exec(ctx,
		`INSERT INTO conversation_messages (id, organization_id, conversation_id, direction, content, created_at)
		 VALUES ($1, (SELECT organization_id FROM conversations WHERE id = $2), $2, $3, $4::jsonb, $5)`,
		message.ID, conversationID, directionWire(direction), string(content), message.CreatedAt); err != nil {
		return StoredMessage{}, fmt.Errorf("postgres: append message: %w", err)
	}
	if _, err := tx.Exec(ctx,
		`UPDATE conversations SET updated_at = $2 WHERE id = $1`,
		conversationID, message.CreatedAt); err != nil {
		return StoredMessage{}, fmt.Errorf("postgres: bump conversation: %w", err)
	}
	if err := tx.Commit(ctx); err != nil {
		return StoredMessage{}, fmt.Errorf("postgres: commit: %w", err)
	}
	return message, nil
}

// ListMessages returns the most recent limit messages for a conversation, oldest first.
// A non-positive limit means "all", matching the in-memory store.
func (s *PostgresStore) ListMessages(ctx context.Context, conversationID string, limit int) ([]StoredMessage, error) {
	if limit <= 0 {
		limit = math.MaxInt32
	}
	rows, err := s.pool.Query(ctx,
		`SELECT id, direction, content->>'text', created_at
		   FROM (SELECT id, direction, content, created_at, seq
		           FROM conversation_messages
		          WHERE conversation_id = $1
		          ORDER BY seq DESC LIMIT $2) recent
		  ORDER BY recent.seq ASC`,
		conversationID, limit)
	if err != nil {
		return nil, fmt.Errorf("postgres: list messages: %w", err)
	}
	defer rows.Close()

	out := []StoredMessage{}
	for rows.Next() {
		var (
			message   StoredMessage
			direction string
			text      *string
		)
		if err := rows.Scan(&message.ID, &direction, &text, &message.CreatedAt); err != nil {
			return nil, fmt.Errorf("postgres: scan message: %w", err)
		}
		message.ConversationID = conversationID
		message.Direction = directionFromWire(direction)
		if text != nil {
			message.Text = *text
		}
		out = append(out, message)
	}
	return out, rows.Err()
}

// ListConversations returns a summary per conversation in scope's org that is visible
// to scope and has at least one message.
//
// SECURITY: both filters — org and owner — are in the SELECT, not applied to an
// already-truncated page in Go. Filtering after a limit hands back short or empty
// pages that read as "no conversations" rather than as a bug. th-8fe998.
func (s *PostgresStore) ListConversations(ctx context.Context, scope ConversationScope) ([]ConversationSummary, error) {
	// The owner predicate is the SQL twin of ConversationScope.Allows: an ownerless
	// conversation is visible to everyone in the org (anonymous and emailless
	// principals must still be able to use the conversation they just created), an
	// owned one only to the matching principal, case-insensitively.
	rows, err := s.pool.Query(ctx,
		`SELECT c.id,
		        c.updated_at,
		        (SELECT count(*) FROM conversation_messages m WHERE m.conversation_id = c.id),
		        (SELECT m.content->>'text' FROM conversation_messages m
		          WHERE m.conversation_id = c.id AND m.direction = 'inbound'
		          ORDER BY m.seq ASC LIMIT 1)
		   FROM conversations c
		  WHERE c.organization_id = $1
		    AND EXISTS (SELECT 1 FROM conversation_messages m WHERE m.conversation_id = c.id)
		    AND ($2 OR EXISTS (
		          SELECT 1 FROM conversation_participants p
		           WHERE p.conversation_id = c.id AND p.type = 'user'
		             AND (coalesce(btrim(p.email), '') = ''
		                  OR (btrim($3) <> '' AND lower(btrim(p.email)) = lower(btrim($3))))))`,
		scope.OrgID, scope.Unscoped, scope.Email)
	if err != nil {
		return nil, fmt.Errorf("postgres: list conversations: %w", err)
	}
	defer rows.Close()

	out := []ConversationSummary{}
	for rows.Next() {
		var (
			summary      ConversationSummary
			messageCount int64
			firstInbound *string
		)
		if err := rows.Scan(&summary.ConversationID, &summary.UpdatedAt, &messageCount, &firstInbound); err != nil {
			return nil, fmt.Errorf("postgres: scan conversation: %w", err)
		}
		summary.MessageCount = int(messageCount)
		if firstInbound != nil {
			summary.FirstInbound = *firstInbound
		}
		out = append(out, summary)
	}
	return out, rows.Err()
}

// SetCurrentStep persists a session's workflow step id. A no-op for an unknown session.
func (s *PostgresStore) SetCurrentStep(ctx context.Context, sessionID, stepID string) error {
	return s.mergeSessionMetadata(ctx, sessionID, map[string]any{"currentStepId": stepID})
}

// SetSessionAuthenticated persists a session's OTP-verified bit. A no-op for an unknown session.
func (s *PostgresStore) SetSessionAuthenticated(ctx context.Context, sessionID string, verified bool) error {
	return s.mergeSessionMetadata(ctx, sessionID, map[string]any{"otpVerified": verified})
}

// AttachSessionContact stamps a captured identity onto a session's metadata (merge — only
// the non-blank fields, so an existing contact is never clobbered). A no-op for an unknown
// session (the UPDATE matches no row).
func (s *PostgresStore) AttachSessionContact(ctx context.Context, sessionID, userName, email, phone string) error {
	patch := map[string]any{}
	if userName != "" {
		patch["userName"] = userName
	}
	if email != "" {
		patch["contactEmail"] = email
	}
	if phone != "" {
		patch["contactPhone"] = phone
	}
	if len(patch) == 0 {
		return nil
	}
	return s.mergeSessionMetadata(ctx, sessionID, patch)
}

// mergeSessionMetadata merges patch into a session's metadata JSON. `||` on jsonb is a
// shallow merge, which is all this flat object needs — and it leaves the other keys
// (contactEmail, the sibling flag) alone instead of clobbering them.
func (s *PostgresStore) mergeSessionMetadata(ctx context.Context, sessionID string, patch map[string]any) error {
	encoded, err := json.Marshal(patch)
	if err != nil {
		return fmt.Errorf("postgres: encode session metadata patch: %w", err)
	}
	if _, err := s.pool.Exec(ctx,
		`UPDATE conversation_sessions
		    SET metadata = coalesce(metadata, '{}'::jsonb) || $2::jsonb, updated_at = now()
		  WHERE session_id = $1`,
		sessionID, string(encoded)); err != nil {
		return fmt.Errorf("postgres: update session metadata: %w", err)
	}
	return nil
}

// ── adminStore ──────────────────────────────────────────────────────────────

// ListConnectors returns an org's connectors sorted by name.
func (s *PostgresStore) ListConnectors(ctx context.Context, orgID string) ([]*connectorConfig, error) {
	rows, err := s.pool.Query(ctx,
		`SELECT id, name, kind, config, enabled, created_at, updated_at
		   FROM connector_configs WHERE org_id = $1 ORDER BY name`,
		orgID)
	if err != nil {
		return nil, fmt.Errorf("postgres: list connectors: %w", err)
	}
	defer rows.Close()

	out := []*connectorConfig{}
	for rows.Next() {
		connector, err := scanConnector(rows, orgID)
		if err != nil {
			return nil, err
		}
		out = append(out, connector)
	}
	return out, rows.Err()
}

// GetConnector returns an org's connector, or (nil, nil) when unknown. A connector
// belonging to ANOTHER org returns (nil, nil) too — the caller renders the same 404,
// so the id space cannot be probed across orgs.
func (s *PostgresStore) GetConnector(ctx context.Context, orgID, id string) (*connectorConfig, error) {
	rows, err := s.pool.Query(ctx,
		`SELECT id, name, kind, config, enabled, created_at, updated_at
		   FROM connector_configs WHERE org_id = $1 AND id = $2`,
		orgID, id)
	if err != nil {
		return nil, fmt.Errorf("postgres: get connector: %w", err)
	}
	defer rows.Close()
	if !rows.Next() {
		return nil, rows.Err()
	}
	return scanConnector(rows, orgID)
}

func scanConnector(rows pgx.Rows, orgID string) (*connectorConfig, error) {
	connector := &connectorConfig{orgID: orgID}
	var config []byte
	if err := rows.Scan(&connector.ID, &connector.Name, &connector.Kind, &config,
		&connector.Enabled, &connector.CreatedAt, &connector.UpdatedAt); err != nil {
		return nil, fmt.Errorf("postgres: scan connector: %w", err)
	}
	if err := json.Unmarshal(config, &connector.Config); err != nil {
		return nil, fmt.Errorf("postgres: decode connector config: %w", err)
	}
	return connector, nil
}

// PutConnector inserts or updates a connector in its org.
func (s *PostgresStore) PutConnector(ctx context.Context, connector *connectorConfig) error {
	config, err := json.Marshal(connector.Config)
	if err != nil {
		return fmt.Errorf("postgres: encode connector config: %w", err)
	}
	if _, err := s.pool.Exec(ctx,
		`INSERT INTO connector_configs (org_id, id, name, kind, config, enabled, created_at, updated_at)
		 VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $8)
		 ON CONFLICT (org_id, id) DO UPDATE SET
		     name = EXCLUDED.name, kind = EXCLUDED.kind, config = EXCLUDED.config,
		     enabled = EXCLUDED.enabled, updated_at = EXCLUDED.updated_at`,
		connector.orgID, connector.ID, connector.Name, connector.Kind, string(config),
		connector.Enabled, connector.CreatedAt, connector.UpdatedAt); err != nil {
		return fmt.Errorf("postgres: put connector: %w", err)
	}
	return nil
}

// DeleteConnector removes an org's connector, reporting whether it existed. A
// cross-org id deletes nothing and reports false.
func (s *PostgresStore) DeleteConnector(ctx context.Context, orgID, id string) (bool, error) {
	tag, err := s.pool.Exec(ctx, `DELETE FROM connector_configs WHERE org_id = $1 AND id = $2`, orgID, id)
	if err != nil {
		return false, fmt.Errorf("postgres: delete connector: %w", err)
	}
	return tag.RowsAffected() > 0, nil
}

// GetSettings returns an org's settings, or (nil, nil) when unset (the caller
// substitutes defaults).
func (s *PostgresStore) GetSettings(ctx context.Context, orgID string) (*agentSettings, error) {
	settings := &agentSettings{OrgID: orgID}
	var tools []byte
	err := s.pool.QueryRow(ctx,
		`SELECT model, system_prompt, default_tools, updated_at FROM agent_settings WHERE org_id = $1`,
		orgID).Scan(&settings.Model, &settings.SystemPrompt, &tools, &settings.UpdatedAt)
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("postgres: get settings: %w", err)
	}
	if err := json.Unmarshal(tools, &settings.DefaultTools); err != nil {
		return nil, fmt.Errorf("postgres: decode default tools: %w", err)
	}
	if settings.DefaultTools == nil {
		settings.DefaultTools = []string{}
	}
	return settings, nil
}

// PutSettings writes an org's settings (one row per org).
func (s *PostgresStore) PutSettings(ctx context.Context, settings *agentSettings) error {
	tools, err := json.Marshal(settings.DefaultTools)
	if err != nil {
		return fmt.Errorf("postgres: encode default tools: %w", err)
	}
	if _, err := s.pool.Exec(ctx,
		`INSERT INTO agent_settings (org_id, model, system_prompt, default_tools, updated_at)
		 VALUES ($1, $2, $3, $4::jsonb, $5)
		 ON CONFLICT (org_id) DO UPDATE SET
		     model = EXCLUDED.model, system_prompt = EXCLUDED.system_prompt,
		     default_tools = EXCLUDED.default_tools, updated_at = EXCLUDED.updated_at`,
		settings.OrgID, settings.Model, settings.SystemPrompt, string(tools), settings.UpdatedAt); err != nil {
		return fmt.Errorf("postgres: put settings: %w", err)
	}
	return nil
}

// ListRuns returns an org's indexing runs, oldest first (insertion order, like the
// in-memory slice).
func (s *PostgresStore) ListRuns(ctx context.Context, orgID string) ([]*indexingRun, error) {
	rows, err := s.pool.Query(ctx,
		`SELECT id, connector_name, status, started_at, finished_at, documents_seen,
		        chunks_indexed, documents_skipped, error
		   FROM indexing_runs WHERE org_id = $1 ORDER BY started_at, id`,
		orgID)
	if err != nil {
		return nil, fmt.Errorf("postgres: list runs: %w", err)
	}
	defer rows.Close()

	out := []*indexingRun{}
	for rows.Next() {
		run := &indexingRun{orgID: orgID}
		var seen, indexed, skipped int64
		if err := rows.Scan(&run.ID, &run.ConnectorName, &run.Status, &run.StartedAt, &run.FinishedAt,
			&seen, &indexed, &skipped, &run.Error); err != nil {
			return nil, fmt.Errorf("postgres: scan run: %w", err)
		}
		run.DocumentsSeen, run.ChunksIndexed, run.DocumentsSkipped = int(seen), int(indexed), int(skipped)
		out = append(out, run)
	}
	return out, rows.Err()
}

// RecordRun inserts or updates an indexing run.
func (s *PostgresStore) RecordRun(ctx context.Context, run *indexingRun) error {
	if _, err := s.pool.Exec(ctx,
		`INSERT INTO indexing_runs (id, org_id, connector_name, status, started_at, finished_at,
		                            documents_seen, chunks_indexed, documents_skipped, error)
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
		 ON CONFLICT (id) DO UPDATE SET
		     status = EXCLUDED.status, finished_at = EXCLUDED.finished_at,
		     documents_seen = EXCLUDED.documents_seen, chunks_indexed = EXCLUDED.chunks_indexed,
		     documents_skipped = EXCLUDED.documents_skipped, error = EXCLUDED.error`,
		run.ID, run.orgID, run.ConnectorName, run.Status, run.StartedAt, run.FinishedAt,
		int64(run.DocumentsSeen), int64(run.ChunksIndexed), int64(run.DocumentsSkipped), run.Error); err != nil {
		return fmt.Errorf("postgres: record run: %w", err)
	}
	return nil
}

// ── helpers ─────────────────────────────────────────────────────────────────

// nullIfBlank maps "" to a SQL NULL so an ownerless conversation stores NULL rather
// than an empty string (the schema's nullable email column, and what Rust writes).
func nullIfBlank(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}

func directionWire(d MessageDirection) string {
	if d == Inbound {
		return "inbound"
	}
	return "outbound"
}

func directionFromWire(s string) MessageDirection {
	if s == "inbound" {
		return Inbound
	}
	return Outbound
}

var (
	_ SessionStore = (*PostgresStore)(nil)
	_ adminStore   = (*PostgresStore)(nil)
)
