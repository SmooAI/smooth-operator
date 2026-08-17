package server

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

// The `/admin/*` API the console drives. Two things matter per route: it must
// fail CLOSED without a sufficient token, and it must answer the wire shape the
// console's typed client expects (camelCase, `{error:{code,message}}`).

// roleVerifier is an auth-enabled verifier that maps a token straight to a role,
// so a test can present "admin" / "curator" / "basic" / an unknown token.
type roleVerifier struct{}

func (roleVerifier) Mode() string { return "test" }

func (roleVerifier) Resolve(token string) AccessContext {
	switch token {
	case "admin", "curator", "basic":
		return AccessContext{
			Principal:   Principal{Sub: "u-" + token, Org: "org-1", Role: token},
			AuthEnabled: true,
		}
	case "other-org-admin":
		return AccessContext{
			Principal:   Principal{Sub: "u-other", Org: "org-2", Role: "admin"},
			AuthEnabled: true,
		}
	default:
		return AccessContext{Principal: AnonymousPrincipal, IsAnonymous: true, AuthEnabled: true}
	}
}

func adminServer(t *testing.T) http.Handler {
	t.Helper()
	return New(WithAuth(roleVerifier{})).Handler()
}

// call issues an admin request with an optional bearer token.
func call(t *testing.T, h http.Handler, method, path, token, body string) *httptest.ResponseRecorder {
	t.Helper()
	var reader *strings.Reader
	if body == "" {
		reader = strings.NewReader("")
	} else {
		reader = strings.NewReader(body)
	}
	req := httptest.NewRequest(method, path, reader)
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	return rec
}

func decode(t *testing.T, rec *httptest.ResponseRecorder) map[string]any {
	t.Helper()
	var out map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &out); err != nil {
		t.Fatalf("decode %q: %v", rec.Body.String(), err)
	}
	return out
}

// Every gated route, with the minimum role it requires. This IS the contract
// table — if a route is added without a gate, it belongs here or it isn't done.
var gatedRoutes = []struct {
	method, path, minRole string
}{
	{"GET", "/admin/me", "basic"},
	{"GET", "/admin/conversations", "basic"},
	{"GET", "/admin/conversations/c1/messages", "basic"},
	{"GET", "/admin/indexing/runs", "curator"},
	{"GET", "/admin/document-sets", "curator"},
	{"GET", "/admin/connectors", "curator"},
	{"POST", "/admin/connectors", "admin"},
	{"POST", "/admin/connectors/x/index", "curator"},
	{"GET", "/admin/connectors/x", "curator"},
	{"PUT", "/admin/connectors/x", "admin"},
	{"DELETE", "/admin/connectors/x", "admin"},
	{"GET", "/admin/settings", "curator"},
	{"PUT", "/admin/settings", "admin"},
}

func TestAdminRoutesFailClosedWithoutAToken(t *testing.T) {
	h := adminServer(t)
	for _, rt := range gatedRoutes {
		rec := call(t, h, rt.method, rt.path, "", `{}`)
		if rec.Code != http.StatusUnauthorized {
			t.Errorf("%s %s without a token = %d, want 401", rt.method, rt.path, rec.Code)
			continue
		}
		body := decode(t, rec)
		errObj, _ := body["error"].(map[string]any)
		if errObj == nil || errObj["code"] != "UNAUTHENTICATED" {
			t.Errorf("%s %s error envelope = %v", rt.method, rt.path, body)
		}
	}
}

func TestAdminRoutesRejectAnInvalidToken(t *testing.T) {
	h := adminServer(t)
	rec := call(t, h, "GET", "/admin/me", "garbage", "")
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("invalid token = %d, want 401", rec.Code)
	}
	if code := decode(t, rec)["error"].(map[string]any)["code"]; code != "INVALID_TOKEN" {
		t.Errorf("code = %v, want INVALID_TOKEN", code)
	}
}

func TestAdminRoutesEnforceRoleRank(t *testing.T) {
	h := adminServer(t)
	// A basic principal may read /admin/me but not curator or admin surfaces.
	if rec := call(t, h, "GET", "/admin/me", "basic", ""); rec.Code != http.StatusOK {
		t.Errorf("basic on /admin/me = %d, want 200", rec.Code)
	}
	if rec := call(t, h, "GET", "/admin/settings", "basic", ""); rec.Code != http.StatusForbidden {
		t.Errorf("basic on GET /admin/settings = %d, want 403", rec.Code)
	}
	// A curator may read settings but not write them.
	if rec := call(t, h, "GET", "/admin/settings", "curator", ""); rec.Code != http.StatusOK {
		t.Errorf("curator on GET /admin/settings = %d, want 200", rec.Code)
	}
	rec := call(t, h, "PUT", "/admin/settings", "curator", `{"model":"m","systemPrompt":"","defaultTools":[]}`)
	if rec.Code != http.StatusForbidden {
		t.Errorf("curator on PUT /admin/settings = %d, want 403", rec.Code)
	}
	if code := decode(t, rec)["error"].(map[string]any)["code"]; code != "FORBIDDEN" {
		t.Errorf("code = %v, want FORBIDDEN", code)
	}
}

func TestAdminHealthIsUngated(t *testing.T) {
	// The console probes health before it has a token.
	rec := call(t, adminServer(t), "GET", "/admin/health", "", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("health = %d, want 200", rec.Code)
	}
}

func TestAdminMeShapesThePrincipal(t *testing.T) {
	body := decode(t, call(t, adminServer(t), "GET", "/admin/me", "curator", ""))
	if body["userId"] != "u-curator" || body["orgId"] != "org-1" || body["role"] != "curator" {
		t.Errorf("me = %v", body)
	}
}

func TestConnectorCrudRoundTrip(t *testing.T) {
	h := adminServer(t)

	// create
	rec := call(t, h, "POST", "/admin/connectors", "admin", `{"name":"docs","kind":"web","config":{"url":"https://x"},"enabled":true}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("create = %d: %s", rec.Code, rec.Body.String())
	}
	created := decode(t, rec)["connector"].(map[string]any)
	id, _ := created["id"].(string)
	if id == "" || created["name"] != "docs" || created["enabled"] != true {
		t.Fatalf("created = %v", created)
	}
	if created["createdAt"] == nil || created["updatedAt"] == nil {
		t.Errorf("timestamps missing: %v", created)
	}

	// list (curator can read)
	list := decode(t, call(t, h, "GET", "/admin/connectors", "curator", ""))["connectors"].([]any)
	if len(list) != 1 {
		t.Fatalf("list = %v", list)
	}

	// get
	got := decode(t, call(t, h, "GET", "/admin/connectors/"+id, "curator", ""))["connector"].(map[string]any)
	if got["id"] != id {
		t.Errorf("get = %v", got)
	}

	// update
	rec = call(t, h, "PUT", "/admin/connectors/"+id, "admin", `{"name":"docs2","kind":"web","config":{},"enabled":false}`)
	updated := decode(t, rec)["connector"].(map[string]any)
	if updated["name"] != "docs2" || updated["enabled"] != false {
		t.Errorf("updated = %v", updated)
	}

	// index trigger records a run
	rec = call(t, h, "POST", "/admin/connectors/"+id+"/index", "curator", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("index = %d: %s", rec.Code, rec.Body.String())
	}
	runs := decode(t, call(t, h, "GET", "/admin/indexing/runs", "curator", ""))["runs"].([]any)
	if len(runs) != 1 {
		t.Errorf("runs = %v", runs)
	}

	// delete → 204, then 404
	if rec = call(t, h, "DELETE", "/admin/connectors/"+id, "admin", ""); rec.Code != http.StatusNoContent {
		t.Errorf("delete = %d, want 204", rec.Code)
	}
	if rec = call(t, h, "GET", "/admin/connectors/"+id, "curator", ""); rec.Code != http.StatusNotFound {
		t.Errorf("get after delete = %d, want 404", rec.Code)
	}
}

func TestConnectorsAreOrgIsolated(t *testing.T) {
	h := adminServer(t)
	rec := call(t, h, "POST", "/admin/connectors", "admin", `{"name":"mine","kind":"web","config":{},"enabled":true}`)
	id := decode(t, rec)["connector"].(map[string]any)["id"].(string)

	// Another org's admin must not see it, and must not be able to tell an
	// existing-but-foreign id from an unknown one.
	if rec := call(t, h, "GET", "/admin/connectors/"+id, "other-org-admin", ""); rec.Code != http.StatusNotFound {
		t.Errorf("cross-org get = %d, want 404", rec.Code)
	}
	list := decode(t, call(t, h, "GET", "/admin/connectors", "other-org-admin", ""))["connectors"].([]any)
	if len(list) != 0 {
		t.Errorf("cross-org list leaked %d connectors", len(list))
	}
}

func TestConnectorCreateValidates(t *testing.T) {
	h := adminServer(t)
	if rec := call(t, h, "POST", "/admin/connectors", "admin", `{"kind":"web"}`); rec.Code != http.StatusBadRequest {
		t.Errorf("missing name = %d, want 400", rec.Code)
	}
	if rec := call(t, h, "POST", "/admin/connectors", "admin", `not json`); rec.Code != http.StatusBadRequest {
		t.Errorf("malformed body = %d, want 400", rec.Code)
	}
}

func TestSettingsDefaultThenRoundTrip(t *testing.T) {
	h := adminServer(t)

	// Unset settings read as defaults rather than 404.
	got := decode(t, call(t, h, "GET", "/admin/settings", "curator", ""))["settings"].(map[string]any)
	if got["orgId"] != "org-1" || got["model"] == "" {
		t.Fatalf("default settings = %v", got)
	}

	rec := call(t, h, "PUT", "/admin/settings", "admin", `{"model":"claude-sonnet-4-5","systemPrompt":"be nice","defaultTools":["search"]}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("put = %d: %s", rec.Code, rec.Body.String())
	}
	saved := decode(t, rec)["settings"].(map[string]any)
	if saved["model"] != "claude-sonnet-4-5" || saved["systemPrompt"] != "be nice" {
		t.Fatalf("saved = %v", saved)
	}
	reread := decode(t, call(t, h, "GET", "/admin/settings", "curator", ""))["settings"].(map[string]any)
	if reread["model"] != "claude-sonnet-4-5" {
		t.Errorf("settings did not persist: %v", reread)
	}
}

func TestConversationsAndMessagesShape(t *testing.T) {
	h := adminServer(t)
	body := decode(t, call(t, h, "GET", "/admin/conversations", "curator", ""))
	if _, ok := body["conversations"].([]any); !ok {
		t.Errorf("conversations = %v", body)
	}
	if _, present := body["nextCursor"]; !present {
		t.Error("nextCursor must be present (null when there is no next page)")
	}

	msgs := decode(t, call(t, h, "GET", "/admin/conversations/c1/messages", "curator", ""))
	if msgs["conversationId"] != "c1" {
		t.Errorf("messages = %v", msgs)
	}
	if _, ok := msgs["messages"].([]any); !ok {
		t.Errorf("messages list missing: %v", msgs)
	}
}

func TestDocumentSetsShape(t *testing.T) {
	body := decode(t, call(t, adminServer(t), "GET", "/admin/document-sets", "curator", ""))
	if _, ok := body["documentSets"].([]any); !ok {
		t.Errorf("documentSets = %v", body)
	}
}

func TestNoAuthDevModeGrantsAdmin(t *testing.T) {
	// AUTH_MODE=none is the local dev flavor, and Rust's NoAuthVerifier returns a
	// fixed Admin principal there. Without the same grant the console 403-walls
	// against a local server — as useless as the 404s this API exists to fix.
	h := New().Handler() // default verifier: PermissiveVerifier, Mode() == "none"
	for _, path := range []string{"/admin/settings", "/admin/connectors", "/admin/indexing/runs"} {
		if rec := call(t, h, "GET", path, "dev-token", ""); rec.Code != http.StatusOK {
			t.Errorf("GET %s on a no-auth server = %d, want 200", path, rec.Code)
		}
	}
	// Still fails closed with NO token at all, even in dev.
	if rec := call(t, h, "GET", "/admin/settings", "", ""); rec.Code != http.StatusUnauthorized {
		t.Errorf("no token on a no-auth server = %d, want 401", rec.Code)
	}
}

func TestAuthEnabledServerIsUnaffectedByTheDevGrant(t *testing.T) {
	// The dev grant must not leak into a server that HAS auth configured.
	h := adminServer(t) // roleVerifier: AuthEnabled, Mode() == "test"
	if rec := call(t, h, "GET", "/admin/settings", "basic", ""); rec.Code != http.StatusForbidden {
		t.Errorf("basic on an auth-enabled server = %d, want 403", rec.Code)
	}
}

// ── model costs ─────────────────────────────────────────────────────────────

func TestMapModelInfoMapsTheGatewayPayload(t *testing.T) {
	// A representative /v1/model/info payload from the LiteLLM gateway.
	payload := map[string]any{"data": []any{
		map[string]any{
			"model_name": "claude-opus-4-8",
			"model_info": map[string]any{
				"input_cost_per_token":  0.000015,
				"output_cost_per_token": 0.000075,
				"model_tier":            "frontier",
				"use_cases":             []any{"reasoning", "coding"},
				"max_output_tokens":     float64(65536),
			},
		},
		// Missing fields stay NULL rather than defaulting to a wrong number —
		// a $0 price would render a free-model badge on a paid model.
		map[string]any{"model_name": "mystery-model", "model_info": map[string]any{}},
		// No model_name → skipped entirely.
		map[string]any{"model_info": map[string]any{"input_cost_per_token": 1.0}},
	}}

	got := mapModelInfo(payload)
	if len(got) != 2 {
		t.Fatalf("mapped %d models, want 2: %v", len(got), got)
	}
	opus := got["claude-opus-4-8"].(map[string]any)
	if opus["inputCostPerToken"] != 0.000015 || opus["outputCostPerToken"] != 0.000075 {
		t.Errorf("costs = %v", opus)
	}
	if opus["tier"] != "frontier" || opus["maxOutputTokens"] != float64(65536) {
		t.Errorf("tier/ceiling = %v", opus)
	}
	if cases, _ := opus["useCases"].([]any); len(cases) != 2 {
		t.Errorf("useCases = %v", opus["useCases"])
	}

	mystery := got["mystery-model"].(map[string]any)
	for _, k := range []string{"inputCostPerToken", "outputCostPerToken", "tier", "maxOutputTokens"} {
		if mystery[k] != nil {
			t.Errorf("%s should be nil when the gateway omits it, got %v", k, mystery[k])
		}
	}
	if cases, _ := mystery["useCases"].([]any); cases == nil || len(cases) != 0 {
		t.Errorf("useCases should be an empty array, not null: %v", mystery["useCases"])
	}
}

func TestMapModelInfoTolteratesGarbage(t *testing.T) {
	if got := mapModelInfo(map[string]any{}); len(got) != 0 {
		t.Errorf("no data key = %v", got)
	}
	if got := mapModelInfo(map[string]any{"data": "not-an-array"}); len(got) != 0 {
		t.Errorf("non-array data = %v", got)
	}
}

func TestModelCostsIsUngatedAndDegradesToEmpty(t *testing.T) {
	// No gateway reachable in a test, so this exercises the degrade path: an
	// unreachable gateway must yield {} with a 200, never a 500 — a missing cost
	// badge beats a broken console page.
	t.Setenv("SMOOAI_GATEWAY_URL", "http://127.0.0.1:1")
	modelCostsCache.mu.Lock()
	modelCostsCache.loaded, modelCostsCache.value = false, nil
	modelCostsCache.mu.Unlock()

	// Ungated: no bearer token at all.
	rec := call(t, adminServer(t), "GET", "/admin/model-costs", "", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("model-costs = %d, want 200", rec.Code)
	}
	if body := decode(t, rec); len(body) != 0 {
		t.Errorf("unreachable gateway should degrade to {}, got %v", body)
	}
	// A failure must NOT be cached, or one blip pins an empty map for the process.
	modelCostsCache.mu.Lock()
	loaded := modelCostsCache.loaded
	modelCostsCache.mu.Unlock()
	if loaded {
		t.Error("a failed fetch must not be cached")
	}
}

// ── realtime publish ────────────────────────────────────────────────────────

func TestPublishDeliversToAnAttachedConnection(t *testing.T) {
	bp := NewInMemoryBackplane()
	got := make(chan map[string]any, 1)
	bp.Attach(context.Background(), "conn-1", func(e map[string]any) { got <- e })
	h := New(WithAuth(roleVerifier{}), WithBackplane(bp)).Handler()

	rec := call(t, h, "POST", "/admin/publish", "admin",
		`{"target":{"type":"connection","id":"conn-1"},"event":{"kind":"job.done"}}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("publish = %d: %s", rec.Code, rec.Body.String())
	}
	if d := decode(t, rec)["delivered"]; d != float64(1) {
		t.Errorf("delivered = %v, want 1", d)
	}
	select {
	case e := <-got:
		if e["kind"] != "job.done" {
			t.Errorf("event = %v", e)
		}
	default:
		t.Error("event never reached the attached sink")
	}
}

func TestPublishReportsZeroForAnUnattachedConnection(t *testing.T) {
	// Truthful zero: the target type IS routable here, the connection just isn't
	// attached. That is a real "delivered: 0", not a lie.
	h := New(WithAuth(roleVerifier{}), WithBackplane(NewInMemoryBackplane())).Handler()
	rec := call(t, h, "POST", "/admin/publish", "admin",
		`{"target":{"type":"connection","id":"nobody"},"event":{}}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("publish = %d", rec.Code)
	}
	if d := decode(t, rec)["delivered"]; d != float64(0) {
		t.Errorf("delivered = %v, want 0", d)
	}
}

func TestPublishRefusesTargetsTheBackplaneCannotRoute(t *testing.T) {
	// The whole point: session/user/org/agent are NOT routable by a connID→sink
	// backplane. Answering {"delivered": 0} would read as "accepted, reached
	// nobody" for an event that was never routable — a 501 says so out loud.
	h := adminServer(t)
	for _, kind := range []string{"session", "user", "org", "agent"} {
		rec := call(t, h, "POST", "/admin/publish", "admin",
			`{"target":{"type":"`+kind+`","id":"x"},"event":{}}`)
		if rec.Code != http.StatusNotImplemented {
			t.Errorf("%s target = %d, want 501", kind, rec.Code)
			continue
		}
		body := decode(t, rec)
		if code := body["error"].(map[string]any)["code"]; code != "UNSUPPORTED_TARGET" {
			t.Errorf("%s code = %v", kind, code)
		}
		if _, leaked := body["delivered"]; leaked {
			t.Errorf("%s must not report a delivered count at all: %v", kind, body)
		}
	}
}

func TestPublishValidatesTheBody(t *testing.T) {
	h := adminServer(t)
	if rec := call(t, h, "POST", "/admin/publish", "admin", `{"target":{"type":"connection"},"event":{}}`); rec.Code != http.StatusBadRequest {
		t.Errorf("missing id = %d, want 400", rec.Code)
	}
	if rec := call(t, h, "POST", "/admin/publish", "admin", `{"target":{"type":"wat","id":"x"}}`); rec.Code != http.StatusBadRequest {
		t.Errorf("unknown type = %d, want 400", rec.Code)
	}
	if rec := call(t, h, "POST", "/admin/publish", "admin", `not json`); rec.Code != http.StatusBadRequest {
		t.Errorf("malformed = %d, want 400", rec.Code)
	}
}

func TestPublishIsAdminGated(t *testing.T) {
	h := adminServer(t)
	if rec := call(t, h, "POST", "/admin/publish", "curator", `{"target":{"type":"connection","id":"x"},"event":{}}`); rec.Code != http.StatusForbidden {
		t.Errorf("curator = %d, want 403", rec.Code)
	}
	if rec := call(t, h, "POST", "/admin/publish", "", `{}`); rec.Code != http.StatusUnauthorized {
		t.Errorf("no token = %d, want 401", rec.Code)
	}
}
