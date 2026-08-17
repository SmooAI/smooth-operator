package server

// The `/admin/*` management API — what the console (console/) drives.
//
// Wire contract is the Rust server's `rust/smooth-operator-server/src/admin.rs`:
// same paths, same camelCase JSON, same `{"error":{"code","message"}}` envelope,
// and the same role gate (Bearer token → verify → rank check; 401 missing/invalid,
// 403 insufficient). Rank: basic=0, curator=1, admin=2.
//
// Connector configs, settings and indexing runs sit behind the adminStore seam
// below. inMemoryAdminStore is the default (this server is memory-only unless
// told otherwise); PostgresStore is the durable implementation, selected with
// SMOOTH_AGENT_STORAGE=postgres. Nothing outside this file and postgres_store.go
// touches either.

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
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

// adminStore is the persistence seam for the three management-console stores. Every
// method takes the caller's org and filters by it, so one org can never see or mutate
// another's rows — the same isolation the Rust handlers get from their storage
// adapter. A cross-org id is reported exactly like an unknown one (nil / false), so
// the handlers render an identical 404 and the id space cannot be probed.
//
// Two implementations: inMemoryAdminStore (default) and PostgresStore (durable).
type adminStore interface {
	ListConnectors(ctx context.Context, orgID string) ([]*connectorConfig, error)
	// GetConnector returns nil (no error) when the org has no such connector.
	GetConnector(ctx context.Context, orgID, id string) (*connectorConfig, error)
	PutConnector(ctx context.Context, connector *connectorConfig) error
	// DeleteConnector reports whether the connector existed in that org.
	DeleteConnector(ctx context.Context, orgID, id string) (bool, error)
	// GetSettings returns nil (no error) when the org has none; the caller substitutes defaults.
	GetSettings(ctx context.Context, orgID string) (*agentSettings, error)
	PutSettings(ctx context.Context, settings *agentSettings) error
	ListRuns(ctx context.Context, orgID string) ([]*indexingRun, error)
	RecordRun(ctx context.Context, run *indexingRun) error
}

// inMemoryAdminStore is the in-process adminStore — the reference implementation.
// Safe for concurrent use.
type inMemoryAdminStore struct {
	mu         sync.Mutex
	connectors map[string]*connectorConfig
	settings   map[string]*agentSettings
	runs       []*indexingRun
}

func newInMemoryAdminStore() *inMemoryAdminStore {
	return &inMemoryAdminStore{
		connectors: map[string]*connectorConfig{},
		settings:   map[string]*agentSettings{},
	}
}

func (s *inMemoryAdminStore) ListConnectors(_ context.Context, orgID string) ([]*connectorConfig, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := []*connectorConfig{}
	for _, c := range s.connectors {
		if c.orgID == orgID {
			out = append(out, c)
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out, nil
}

func (s *inMemoryAdminStore) GetConnector(_ context.Context, orgID, id string) (*connectorConfig, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	// A cross-org id takes the same branch as an unknown one.
	if c, found := s.connectors[id]; found && c.orgID == orgID {
		clone := *c
		return &clone, nil
	}
	return nil, nil
}

func (s *inMemoryAdminStore) PutConnector(_ context.Context, connector *connectorConfig) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	clone := *connector
	s.connectors[connector.ID] = &clone
	return nil
}

func (s *inMemoryAdminStore) DeleteConnector(_ context.Context, orgID, id string) (bool, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if c, found := s.connectors[id]; !found || c.orgID != orgID {
		return false, nil
	}
	delete(s.connectors, id)
	return true, nil
}

func (s *inMemoryAdminStore) GetSettings(_ context.Context, orgID string) (*agentSettings, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if settings, found := s.settings[orgID]; found {
		clone := *settings
		return &clone, nil
	}
	return nil, nil
}

func (s *inMemoryAdminStore) PutSettings(_ context.Context, settings *agentSettings) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	clone := *settings
	s.settings[settings.OrgID] = &clone
	return nil
}

func (s *inMemoryAdminStore) ListRuns(_ context.Context, orgID string) ([]*indexingRun, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := []*indexingRun{}
	for _, run := range s.runs {
		if run.orgID == orgID {
			out = append(out, run)
		}
	}
	return out, nil
}

func (s *inMemoryAdminStore) RecordRun(_ context.Context, run *indexingRun) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	for i, existing := range s.runs {
		if existing.ID == run.ID {
			clone := *run
			s.runs[i] = &clone
			return nil
		}
	}
	clone := *run
	s.runs = append(s.runs, &clone)
	return nil
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

	mux.HandleFunc("GET /admin/model-costs", s.adminModelCosts)

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

	mux.HandleFunc("POST /admin/publish", s.adminPublish)

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
	runs, err := s.admin.ListRuns(r.Context(), principal.Org)
	if err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not list indexing runs")
		return
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
	out, err := s.admin.ListConnectors(r.Context(), principal.Org)
	if err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not list connectors")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"connectors": out})
}

// connectorFor returns an org-owned connector, or writes the rejection and returns
// ok=false. A cross-org id is deliberately indistinguishable from an unknown one:
// both are a plain 404, so the connector id space cannot be probed across orgs.
func (s *Server) connectorFor(w http.ResponseWriter, r *http.Request, id, orgID string) (*connectorConfig, bool) {
	c, err := s.admin.GetConnector(r.Context(), orgID, id)
	if err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not read connector")
		return nil, false
	}
	if c == nil {
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
	c, found := s.connectorFor(w, r, r.PathValue("id"), principal.Org)
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
	if err := s.admin.PutConnector(r.Context(), c); err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not create connector")
		return
	}
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
	// ponytail: read-modify-write without a lock across the two calls. Concurrent PUTs
	// to the SAME connector are last-write-wins, which is what a Postgres upsert does
	// anyway; add row-level locking only if a real conflicting-editor case shows up.
	c, found := s.connectorFor(w, r, r.PathValue("id"), principal.Org)
	if !found {
		return
	}
	c.Name, c.Kind, c.Config, c.Enabled = body.Name, body.Kind, body.Config, body.Enabled
	c.UpdatedAt = time.Now().UTC()
	if err := s.admin.PutConnector(r.Context(), c); err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not update connector")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"connector": c})
}

func (s *Server) adminDeleteConnector(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleAdmin)
	if !ok {
		return
	}
	deleted, err := s.admin.DeleteConnector(r.Context(), principal.Org, r.PathValue("id"))
	if err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not delete connector")
		return
	}
	if !deleted {
		// Unknown and cross-org are the same 404 — no existence oracle.
		writeAdminError(w, http.StatusNotFound, "NOT_FOUND", "connector not found")
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *Server) adminIndexConnector(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleCurator)
	if !ok {
		return
	}
	c, found := s.connectorFor(w, r, r.PathValue("id"), principal.Org)
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
	if err := s.admin.RecordRun(r.Context(), run); err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not record indexing run")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"run": run})
}

func (s *Server) adminGetSettings(w http.ResponseWriter, r *http.Request) {
	principal, ok := s.requireRole(w, r, roleCurator)
	if !ok {
		return
	}
	settings, err := s.admin.GetSettings(r.Context(), principal.Org)
	if err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not read settings")
		return
	}
	if settings == nil {
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
	if err := s.admin.PutSettings(r.Context(), settings); err != nil {
		writeAdminError(w, http.StatusInternalServerError, "INTERNAL", "could not save settings")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"settings": settings})
}

// ── model costs ─────────────────────────────────────────────────────────────

// modelCostsCache holds the mapped /model/info payload for the process. The
// gateway's pricing is stable, so one fetch per process is enough — matching the
// Rust server's OnceCell. Only a SUCCESS is cached; an error leaves it unset so
// the next request retries.
var modelCostsCache struct {
	mu     sync.Mutex
	value  map[string]any
	loaded bool
}

// adminModelCosts serves GET /admin/model-costs.
//
// UNGATED, exactly as in Rust: gateway pricing is not org-sensitive and the
// console's cost badges must render on a tokenless local connection. Any gateway
// or transport failure degrades to an empty object rather than a 500 — a missing
// badge is better than a broken page.
func (s *Server) adminModelCosts(w http.ResponseWriter, r *http.Request) {
	modelCostsCache.mu.Lock()
	if modelCostsCache.loaded {
		cached := modelCostsCache.value
		modelCostsCache.mu.Unlock()
		writeJSON(w, http.StatusOK, cached)
		return
	}
	modelCostsCache.mu.Unlock()

	costs, err := fetchModelCosts(r.Context())
	if err != nil {
		writeJSON(w, http.StatusOK, map[string]any{})
		return
	}
	modelCostsCache.mu.Lock()
	modelCostsCache.value, modelCostsCache.loaded = costs, true
	modelCostsCache.mu.Unlock()
	writeJSON(w, http.StatusOK, costs)
}

// fetchModelCosts GETs the gateway's /model/info with the server's configured
// gateway credentials — the same ones the turns use — and maps it.
func fetchModelCosts(ctx context.Context) (map[string]any, error) {
	base := strings.TrimRight(orDefaultEnv("SMOOAI_GATEWAY_URL", "https://llm.smoo.ai/v1"), "/")
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, base+"/model/info", nil)
	if err != nil {
		return nil, err
	}
	if key := strings.TrimSpace(os.Getenv("SMOOAI_GATEWAY_KEY")); key != "" {
		req.Header.Set("Authorization", "Bearer "+key)
	}
	resp, err := (&http.Client{Timeout: 10 * time.Second}).Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return nil, fmt.Errorf("model/info: %d", resp.StatusCode)
	}
	var payload map[string]any
	if err := json.NewDecoder(resp.Body).Decode(&payload); err != nil {
		return nil, err
	}
	return mapModelInfo(payload), nil
}

func orDefaultEnv(name, fallback string) string {
	if v := strings.TrimSpace(os.Getenv(name)); v != "" {
		return v
	}
	return fallback
}

// mapModelInfo turns the gateway's /model/info payload into the
// `{ "<model>": {inputCostPerToken, outputCostPerToken, tier, useCases,
// maxOutputTokens} }` shape the console reads. Pure, so it is unit-testable on a
// sample payload without a gateway. Mirrors the Rust `map_model_info`: entries
// without a `model_name` are skipped, and every field is optional (null when the
// gateway omits it) rather than defaulted to a wrong number.
func mapModelInfo(payload map[string]any) map[string]any {
	out := map[string]any{}
	entries, _ := payload["data"].([]any)
	for _, raw := range entries {
		entry, _ := raw.(map[string]any)
		name, _ := entry["model_name"].(string)
		if name == "" {
			continue
		}
		info, _ := entry["model_info"].(map[string]any)
		out[name] = map[string]any{
			"inputCostPerToken":  numOrNil(info, "input_cost_per_token"),
			"outputCostPerToken": numOrNil(info, "output_cost_per_token"),
			"tier":               strOrNil(info, "model_tier"),
			"useCases":           arrOrEmpty(info, "use_cases"),
			"maxOutputTokens":    numOrNil(info, "max_output_tokens"),
		}
	}
	return out
}

func numOrNil(info map[string]any, key string) any {
	if v, ok := info[key].(float64); ok {
		return v
	}
	return nil
}

func strOrNil(info map[string]any, key string) any {
	if v, ok := info[key].(string); ok {
		return v
	}
	return nil
}

func arrOrEmpty(info map[string]any, key string) []any {
	if v, ok := info[key].([]any); ok {
		return v
	}
	return []any{}
}

// ── realtime publish ────────────────────────────────────────────────────────

// publishRequest is the POST /admin/publish body. `target` is the friendlier
// `{type, id}` shape the Rust server accepts.
type publishRequest struct {
	Target struct {
		Type string `json:"type"`
		ID   string `json:"id"`
	} `json:"target"`
	Event map[string]any `json:"event"`
}

// adminPublish serves POST /admin/publish — push a realtime event to a target
// over the connection fleet. The plug point for non-AI publishers (job status,
// ingestion progress, notifications) that need to reach a connected client
// without going through an agent turn. Admin-gated.
//
// This server's backplane is a connID→sink registry, so only `connection`
// targets can be routed. Rust additionally fans out to session/user/org/agent
// over a richer backplane; here those return 501 rather than a misleading
// `{"delivered": 0}` — a caller must never read "accepted, reached nobody" as
// success for an event that was never routable in the first place.
func (s *Server) adminPublish(w http.ResponseWriter, r *http.Request) {
	if _, ok := s.requireRole(w, r, roleAdmin); !ok {
		return
	}
	var body publishRequest
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeAdminError(w, http.StatusBadRequest, "INVALID_BODY", "malformed JSON body")
		return
	}
	kind, id := strings.ToLower(strings.TrimSpace(body.Target.Type)), strings.TrimSpace(body.Target.ID)
	if id == "" {
		writeAdminError(w, http.StatusBadRequest, "INVALID_BODY", "target.id is required")
		return
	}
	switch kind {
	case "connection":
		delivered := s.backplane.Publish(r.Context(), id, body.Event)
		writeJSON(w, http.StatusOK, map[string]any{"delivered": delivered})
	case "session", "user", "org", "agent":
		writeAdminError(w, http.StatusNotImplemented, "UNSUPPORTED_TARGET",
			fmt.Sprintf("this server's backplane routes by connection id only; %q targets are not deliverable here", kind))
	default:
		writeAdminError(w, http.StatusBadRequest, "INVALID_BODY",
			fmt.Sprintf("unknown target type %q (want connection|session|user|org|agent)", kind))
	}
}
