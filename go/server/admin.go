package server

// The `/admin/*` management API — what the console (console/) drives.
//
// Wire contract is the Rust server's `rust/smooth-operator-server/src/admin.rs`:
// same paths, same camelCase JSON, same `{"error":{"code","message"}}` envelope,
// and the same role gate (Bearer token → verify → rank check; 401 missing/invalid,
// 403 insufficient). Rank: basic=0, curator=1, admin=2.
//
// ponytail: connector configs, settings and indexing runs are held in memory
// (adminStores below) because this server is memory-only today. The durable
// storage adapter is a separate workstream — swap adminStores' three maps for it;
// nothing outside this file reads them.

import (
	"encoding/json"
	"fmt"
	"net/http"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/google/uuid"
)

// Role ranks, mirroring Rust's `role_rank`.
const (
	roleBasic   = 0
	roleCurator = 1
	roleAdmin   = 2
)

// roleRank maps a principal's role string to its rank. Unknown/empty roles are
// basic — fail closed on privilege, not open.
func roleRank(role string) int {
	switch strings.ToLower(strings.TrimSpace(role)) {
	case "admin":
		return roleAdmin
	case "curator":
		return roleCurator
	default:
		return roleBasic
	}
}

func rankName(rank int) string {
	switch rank {
	case roleAdmin:
		return "admin"
	case roleCurator:
		return "curator"
	default:
		return "basic"
	}
}

// ── in-memory admin state ───────────────────────────────────────────────────

type connectorConfig struct {
	ID        string         `json:"id"`
	Name      string         `json:"name"`
	Kind      string         `json:"kind"`
	Config    map[string]any `json:"config"`
	Enabled   bool           `json:"enabled"`
	CreatedAt time.Time      `json:"createdAt"`
	UpdatedAt time.Time      `json:"updatedAt"`
	orgID     string
}

type connectorWrite struct {
	Name    string         `json:"name"`
	Kind    string         `json:"kind"`
	Config  map[string]any `json:"config"`
	Enabled bool           `json:"enabled"`
}

type agentSettings struct {
	OrgID        string    `json:"orgId"`
	Model        string    `json:"model"`
	SystemPrompt string    `json:"systemPrompt"`
	DefaultTools []string  `json:"defaultTools"`
	UpdatedAt    time.Time `json:"updatedAt"`
}

type settingsWrite struct {
	Model        string   `json:"model"`
	SystemPrompt string   `json:"systemPrompt"`
	DefaultTools []string `json:"defaultTools"`
}

type indexingRun struct {
	ID               string     `json:"id"`
	ConnectorName    string     `json:"connectorName"`
	Status           string     `json:"status"`
	StartedAt        time.Time  `json:"startedAt"`
	FinishedAt       *time.Time `json:"finishedAt"`
	DocumentsSeen    int        `json:"documentsSeen"`
	ChunksIndexed    int        `json:"chunksIndexed"`
	DocumentsSkipped int        `json:"documentsSkipped"`
	Error            *string    `json:"error"`
	orgID            string
}

// adminStores is the org-scoped admin state. Every read and write filters by org,
// so one org can never see or mutate another's rows — the same isolation the Rust
// handlers get from their storage adapter.
type adminStores struct {
	mu         sync.Mutex
	connectors map[string]*connectorConfig
	settings   map[string]*agentSettings
	runs       []*indexingRun
}

func newAdminStores() *adminStores {
	return &adminStores{
		connectors: map[string]*connectorConfig{},
		settings:   map[string]*agentSettings{},
	}
}

// defaultSettings mirrors Rust's "defaults when unset" read.
func defaultSettings(orgID string) *agentSettings {
	return &agentSettings{
		OrgID:        orgID,
		Model:        "claude-haiku-4-5",
		SystemPrompt: "",
		DefaultTools: []string{},
		UpdatedAt:    time.Now().UTC(),
	}
}

// ── error envelope ──────────────────────────────────────────────────────────

func writeAdminError(w http.ResponseWriter, status int, code, message string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]any{
		"error": map[string]string{"code": code, "message": message},
	})
}

func writeJSON(w http.ResponseWriter, status int, body any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(body)
}

// ── auth gate ───────────────────────────────────────────────────────────────

// bearerToken extracts the raw token from `Authorization: Bearer <token>`,
// returning "" when absent or not a bearer scheme (matching Rust's parser,
// case-insensitive scheme included).
func bearerToken(r *http.Request) string {
	value := r.Header.Get("Authorization")
	for _, prefix := range []string{"Bearer ", "bearer "} {
		if rest, ok := strings.CutPrefix(value, prefix); ok {
			return strings.TrimSpace(rest)
		}
	}
	return ""
}

// requireRole authenticates the request and enforces a minimum role. Returns the
// principal, or writes the rejection and returns ok=false. Fails CLOSED: no token
// is 401 even when the configured verifier is permissive.
func (s *Server) requireRole(w http.ResponseWriter, r *http.Request, min int) (Principal, bool) {
	token := bearerToken(r)
	if token == "" {
		writeAdminError(w, http.StatusUnauthorized, "UNAUTHENTICATED", "missing bearer token")
		return Principal{}, false
	}
	access := s.auth.Resolve(token)
	// Fail closed: an auth-enabled server that could not verify the token yields an
	// anonymous context, which must never satisfy an admin route.
	if access.AuthEnabled && access.IsAnonymous {
		writeAdminError(w, http.StatusUnauthorized, "INVALID_TOKEN", "invalid bearer token")
		return Principal{}, false
	}
	principal := access.Principal
	// AUTH_MODE=none (dev) grants Admin, exactly as Rust's NoAuthVerifier does —
	// otherwise the console 403-walls against a local server, which is as useless
	// as the 404s this API exists to fix. Only the explicitly-unauthenticated dev
	// verifier takes this path; an auth-enabled server is unaffected.
	if s.auth.Mode() == "none" {
		principal.Role = "admin"
	}
	if roleRank(principal.Role) < min {
		writeAdminError(w, http.StatusForbidden, "FORBIDDEN",
			fmt.Sprintf("requires role %s, principal has %s", rankName(min), rankName(roleRank(principal.Role))))
		return Principal{}, false
	}
	return principal, true
}

// ── routes ──────────────────────────────────────────────────────────────────

// registerAdminRoutes mounts the `/admin/*` surface onto mux.
func (s *Server) registerAdminRoutes(mux *http.ServeMux) {
	// Ungated, exactly as in Rust: the console must render a health probe on a
	// tokenless local connection.
	mux.HandleFunc("GET /admin/health", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]any{"status": "ok"})
	})

	mux.HandleFunc("GET /admin/me", s.adminMe)
	mux.HandleFunc("GET /admin/conversations", s.adminListConversations)
	mux.HandleFunc("GET /admin/conversations/{id}/messages", s.adminConversationMessages)
	mux.HandleFunc("GET /admin/indexing/runs", s.adminIndexingRuns)
	mux.HandleFunc("GET /admin/document-sets", s.adminDocumentSets)

	mux.HandleFunc("GET /admin/connectors", s.adminListConnectors)
	mux.HandleFunc("POST /admin/connectors", s.adminCreateConnector)
	mux.HandleFunc("GET /admin/connectors/{id}", s.adminGetConnector)
	mux.HandleFunc("PUT /admin/connectors/{id}", s.adminUpdateConnector)
	mux.HandleFunc("DELETE /admin/connectors/{id}", s.adminDeleteConnector)
	mux.HandleFunc("POST /admin/connectors/{id}/index", s.adminIndexConnector)

	mux.HandleFunc("GET /admin/settings", s.adminGetSettings)
	mux.HandleFunc("PUT /admin/settings", s.adminPutSettings)
}

func (s *Server) adminMe(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleBasic)
	if !ok {
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"userId": principal.Sub,
		"orgId":  principal.Org,
		"role":   rankName(roleRank(principal.Role)),
	})
}

func (s *Server) adminListConversations(w http.ResponseWriter, r *http.Request) {
	if _, ok := s.requireRole(w, r, roleBasic); !ok {
		return
	}
	limit := 50
	if v := r.URL.Query().Get("limit"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			limit = n
		}
	}
	cursor := 0
	if v := r.URL.Query().Get("cursor"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n >= 0 {
			cursor = n
		}
	}

	summaries, err := s.store.ListConversations(r.Context(), s.auth.Resolve(bearerToken(r)).ConversationScope())
	if err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not list conversations")
		return
	}
	sort.Slice(summaries, func(i, j int) bool { return summaries[i].UpdatedAt.After(summaries[j].UpdatedAt) })

	rows := make([]map[string]any, 0, limit)
	end := cursor
	for i := cursor; i < len(summaries) && len(rows) < limit; i++ {
		c := summaries[i]
		name := c.FirstInbound
		if name == "" {
			name = "Conversation"
		}
		rows = append(rows, map[string]any{
			"id":        c.ConversationID,
			"name":      name,
			"platform":  "web",
			"createdAt": c.UpdatedAt.UTC(),
			"updatedAt": c.UpdatedAt.UTC(),
		})
		end = i + 1
	}
	var next *int
	if end < len(summaries) {
		next = &end
	}
	writeJSON(w, http.StatusOK, map[string]any{"conversations": rows, "nextCursor": next})
}

func (s *Server) adminConversationMessages(w http.ResponseWriter, r *http.Request) {
	if _, ok := s.requireRole(w, r, roleBasic); !ok {
		return
	}
	id := r.PathValue("id")
	stored, err := s.store.ListMessages(r.Context(), id, 500)
	if err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not list messages")
		return
	}
	messages := make([]map[string]any, 0, len(stored))
	for _, m := range stored {
		direction := "outbound"
		if m.Direction == Inbound {
			direction = "inbound"
		}
		messages = append(messages, map[string]any{
			"id":             m.ID,
			"conversationId": m.ConversationID,
			"direction":      direction,
			"content":        map[string]any{"items": []any{map[string]any{"type": "text", "text": m.Text}}, "text": m.Text},
			"createdAt":      m.CreatedAt.UTC(),
		})
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"conversationId": id,
		"messages":       messages,
		"nextCursor":     nil,
	})
}

func (s *Server) adminIndexingRuns(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleCurator)
	if !ok {
		return
	}
	s.admin.mu.Lock()
	defer s.admin.mu.Unlock()
	runs := make([]*indexingRun, 0)
	for _, run := range s.admin.runs {
		if run.orgID == principal.Org {
			runs = append(runs, run)
		}
	}
	writeJSON(w, http.StatusOK, map[string]any{"runs": runs})
}

func (s *Server) adminDocumentSets(w http.ResponseWriter, r *http.Request) {
	if _, ok := s.requireRole(w, r, roleCurator); !ok {
		return
	}
	// ponytail: this server has no knowledge store yet, so there are no document
	// sets to count. An empty list is the honest answer and renders fine; wire it
	// to the knowledge base when one lands.
	writeJSON(w, http.StatusOK, map[string]any{"documentSets": []any{}})
}

func (s *Server) adminListConnectors(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleCurator)
	if !ok {
		return
	}
	s.admin.mu.Lock()
	defer s.admin.mu.Unlock()
	out := make([]*connectorConfig, 0)
	for _, c := range s.admin.connectors {
		if c.orgID == principal.Org {
			out = append(out, c)
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	writeJSON(w, http.StatusOK, map[string]any{"connectors": out})
}

// connectorFor returns an org-owned connector, or writes a 404. A cross-org id is
// deliberately indistinguishable from an unknown one.
func (s *Server) connectorFor(w http.ResponseWriter, id, orgID string) (*connectorConfig, bool) {
	c, found := s.admin.connectors[id]
	if !found || c.orgID != orgID {
		writeAdminError(w, http.StatusNotFound, "NOT_FOUND", "connector not found")
		return nil, false
	}
	return c, true
}

func (s *Server) adminGetConnector(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleCurator)
	if !ok {
		return
	}
	s.admin.mu.Lock()
	defer s.admin.mu.Unlock()
	c, found := s.connectorFor(w, r.PathValue("id"), principal.Org)
	if !found {
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"connector": c})
}

// decodeConnectorWrite parses and validates a connector body.
func decodeConnectorWrite(w http.ResponseWriter, r *http.Request) (connectorWrite, bool) {
	var body connectorWrite
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeAdminError(w, http.StatusBadRequest, "INVALID_BODY", "malformed JSON body")
		return body, false
	}
	if strings.TrimSpace(body.Name) == "" || strings.TrimSpace(body.Kind) == "" {
		writeAdminError(w, http.StatusBadRequest, "INVALID_BODY", "name and kind are required")
		return body, false
	}
	if body.Config == nil {
		body.Config = map[string]any{}
	}
	return body, true
}

func (s *Server) adminCreateConnector(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleAdmin)
	if !ok {
		return
	}
	body, ok := decodeConnectorWrite(w, r)
	if !ok {
		return
	}
	now := time.Now().UTC()
	c := &connectorConfig{
		ID: uuid.NewString(), Name: body.Name, Kind: body.Kind, Config: body.Config,
		Enabled: body.Enabled, CreatedAt: now, UpdatedAt: now, orgID: principal.Org,
	}
	s.admin.mu.Lock()
	s.admin.connectors[c.ID] = c
	s.admin.mu.Unlock()
	writeJSON(w, http.StatusOK, map[string]any{"connector": c})
}

func (s *Server) adminUpdateConnector(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleAdmin)
	if !ok {
		return
	}
	body, ok := decodeConnectorWrite(w, r)
	if !ok {
		return
	}
	s.admin.mu.Lock()
	defer s.admin.mu.Unlock()
	c, found := s.connectorFor(w, r.PathValue("id"), principal.Org)
	if !found {
		return
	}
	c.Name, c.Kind, c.Config, c.Enabled = body.Name, body.Kind, body.Config, body.Enabled
	c.UpdatedAt = time.Now().UTC()
	writeJSON(w, http.StatusOK, map[string]any{"connector": c})
}

func (s *Server) adminDeleteConnector(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleAdmin)
	if !ok {
		return
	}
	s.admin.mu.Lock()
	defer s.admin.mu.Unlock()
	if _, found := s.connectorFor(w, r.PathValue("id"), principal.Org); !found {
		return
	}
	delete(s.admin.connectors, r.PathValue("id"))
	w.WriteHeader(http.StatusNoContent)
}

func (s *Server) adminIndexConnector(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleCurator)
	if !ok {
		return
	}
	s.admin.mu.Lock()
	defer s.admin.mu.Unlock()
	c, found := s.connectorFor(w, r.PathValue("id"), principal.Org)
	if !found {
		return
	}
	// ponytail: no ingestion pipeline on this server yet, so the run is recorded
	// as succeeded with zero documents rather than faked with invented counts.
	// Point this at the indexer when one lands.
	now := time.Now().UTC()
	finished := now
	run := &indexingRun{
		ID: uuid.NewString(), ConnectorName: c.Name, Status: "succeeded",
		StartedAt: now, FinishedAt: &finished, orgID: principal.Org,
	}
	s.admin.runs = append(s.admin.runs, run)
	writeJSON(w, http.StatusOK, map[string]any{"run": run})
}

func (s *Server) adminGetSettings(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleCurator)
	if !ok {
		return
	}
	s.admin.mu.Lock()
	defer s.admin.mu.Unlock()
	settings, found := s.admin.settings[principal.Org]
	if !found {
		settings = defaultSettings(principal.Org)
	}
	writeJSON(w, http.StatusOK, map[string]any{"settings": settings})
}

func (s *Server) adminPutSettings(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleAdmin)
	if !ok {
		return
	}
	var body settingsWrite
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeAdminError(w, http.StatusBadRequest, "INVALID_BODY", "malformed JSON body")
		return
	}
	if strings.TrimSpace(body.Model) == "" {
		writeAdminError(w, http.StatusBadRequest, "INVALID_BODY", "model is required")
		return
	}
	if body.DefaultTools == nil {
		body.DefaultTools = []string{}
	}
	settings := &agentSettings{
		OrgID: principal.Org, Model: body.Model, SystemPrompt: body.SystemPrompt,
		DefaultTools: body.DefaultTools, UpdatedAt: time.Now().UTC(),
	}
	s.admin.mu.Lock()
	s.admin.settings[principal.Org] = settings
	s.admin.mu.Unlock()
	writeJSON(w, http.StatusOK, map[string]any{"settings": settings})
}
