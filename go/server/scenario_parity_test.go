package server

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"testing"
	"time"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/SmooAI/smooth-operator/go/protocol"
)

// Scenario parity runner — the Go port of the Python reference runner
// (python/server/tests/test_scenario_parity.py).
//
// It runs every scenario in spec/conformance/scenarios/*.json through the Go server
// and asserts the normalized protocol output matches. This is the shared corpus that
// holds the five native servers (Rust · C# · Python · TypeScript · Go) to parity:
// each language's server runs the SAME JSON scenarios through its own server and
// asserts the SAME normalized output. When all five run this corpus green, the
// servers are at protocol parity.
//
// The turn is deterministic because the engine runs on the same MockLlmProvider
// script the scenario declares — no gateway, no flakiness.

// scenario is the on-disk shape of a *.json conformance scenario.
type scenario struct {
	Name          string           `json:"name"`
	Description   string           `json:"description"`
	Server        scenarioServer   `json:"server"`
	MockLlmScript []mockScriptStep `json:"mockLlmScript"`
	Steps         []scenarioStep   `json:"steps"`
}

// scenarioServer is the optional `server` directive: deployment-time config the runner
// applies when starting the server — the tools the agent may call, and the subset gated
// behind write-confirmation HITL.
type scenarioServer struct {
	Tools []toolSpec `json:"tools"`
	// ConfirmTools are tool-name substrings gated behind write-confirmation HITL — a
	// turn that calls a matching tool parks and emits write_confirmation_required until
	// the client replies with confirm_tool_action.
	ConfirmTools []string `json:"confirmTools"`
	// Knowledge seeds the server's knowledge base before booting — each doc grounds the
	// agent AND sources the turn's auto-context citations, so a grounded turn's
	// eventual_response carries data.data.citations.
	Knowledge []knowledgeSpec `json:"knowledge"`
}

// knowledgeSpec is one document a scenario seeds via `server.knowledge`. source is the
// citation id/title (and url when it's an http(s) link); content is the chunk text.
type knowledgeSpec struct {
	Source  string `json:"source"`
	Content string `json:"content"`
}

// toolSpec is one deterministic test tool a scenario registers via `server.tools`. The
// tool ignores its arguments and returns the fixed Result, so a tool-call turn is fully
// reproducible across every native server.
type toolSpec struct {
	Name        string         `json:"name"`
	Description string         `json:"description"`
	Parameters  map[string]any `json:"parameters"`
	Result      string         `json:"result"`
}

type mockScriptStep struct {
	Kind      string `json:"kind"`
	Text      string `json:"text"`
	ID        string `json:"id"`
	Name      string `json:"name"`
	Arguments string `json:"arguments"`
}

type scenarioStep struct {
	Send   map[string]any `json:"send"`
	Expect []matcher      `json:"expect"`
}

// matcher is one expected outbound event in a step's ordered `expect` sequence.
type matcher struct {
	Type              string            `json:"type"`
	Status            *int              `json:"status"`
	StatusGte         *int              `json:"statusGte"`
	Capture           map[string]string `json:"capture"`
	Assert            map[string]any    `json:"assert"`
	Repeat            bool              `json:"repeat"`
	Accumulate        string            `json:"accumulate"`
	AssertAccumulated *string           `json:"assertAccumulated"`
}

// scenariosDir resolves spec/conformance/scenarios relative to the repo root (this
// file lives at go/server/, so the root is three parents up).
func scenariosDir(t *testing.T) string {
	t.Helper()
	wd, err := os.Getwd()
	if err != nil {
		t.Fatalf("getwd: %v", err)
	}
	return filepath.Join(wd, "..", "..", "spec", "conformance", "scenarios")
}

// dot resolves a dotted path into a nested value. A path segment indexes a map by key
// ("data.data.response") or, when it parses as a non-negative integer, an array by
// position ("citations.0.id") — so a citation field can be asserted by index. Mirrors
// the Python reference runner's array-aware dot helper.
func dot(t *testing.T, obj map[string]any, path string) (any, bool) {
	t.Helper()
	var cur any = obj
	for _, part := range strings.Split(path, ".") {
		switch node := cur.(type) {
		case map[string]any:
			v, ok := node[part]
			if !ok {
				return nil, false
			}
			cur = v
		case []any:
			idx, err := strconv.Atoi(part)
			if err != nil || idx < 0 || idx >= len(node) {
				return nil, false
			}
			cur = node[idx]
		default:
			return nil, false
		}
	}
	return cur, true
}

// buildMock loads a scenario's mockLlmScript into the engine's MockLlmProvider — the
// deterministic record/replay source that makes the turn identical across languages.
func buildMock(t *testing.T, script []mockScriptStep) *core.MockLlmProvider {
	t.Helper()
	mock := core.NewMockLlmProvider()
	for _, entry := range script {
		switch entry.Kind {
		case "text":
			mock.PushText(entry.Text)
		case "toolCall":
			id := entry.ID
			if id == "" {
				id = "call-1"
			}
			mock.PushToolCall(id, entry.Name, entry.Arguments)
		default:
			t.Fatalf("unknown mockLlmScript kind: %q", entry.Kind)
		}
	}
	return mock
}

// buildTools turns a scenario's `server.tools` directive into engine tools the agent
// can call. Each tool ignores its arguments and returns the spec's fixed Result, so the
// tool-call turn is fully deterministic across every native server.
func buildTools(specs []toolSpec) []core.Tool {
	if len(specs) == 0 {
		return nil
	}
	tools := make([]core.Tool, 0, len(specs))
	for _, spec := range specs {
		params := spec.Parameters
		if params == nil {
			params = map[string]any{"type": "object", "properties": map[string]any{}}
		}
		result := spec.Result
		tools = append(tools, core.FuncTool{
			ToolName: spec.Name,
			Desc:     spec.Description,
			Params:   params,
			Fn: func(_ context.Context, _ map[string]any) (string, error) {
				return result, nil
			},
		})
	}
	return tools
}

// buildKnowledge turns a scenario's `server.knowledge` directive into a seeded
// InMemoryKnowledge the agent grounds on. Returns nil when no docs are declared, so a
// scenario without knowledge boots a server with no retriever (citations stay empty).
func buildKnowledge(specs []knowledgeSpec) core.Knowledge {
	if len(specs) == 0 {
		return nil
	}
	kb := &core.InMemoryKnowledge{}
	for _, spec := range specs {
		kb.Ingest(spec.Content, spec.Source)
	}
	return kb
}

// subst replaces "{{name}}" placeholders in string fields from captured vars. A whole
// string value of exactly "{{name}}" resolves to the captured value (matching the
// Python reference's _subst, which only substitutes full-field placeholders).
func subst(value any, vars map[string]any) any {
	switch v := value.(type) {
	case string:
		if strings.HasPrefix(v, "{{") && strings.HasSuffix(v, "}}") {
			return vars[v[2:len(v)-2]]
		}
		return v
	case map[string]any:
		out := make(map[string]any, len(v))
		for k, vv := range v {
			out[k] = subst(vv, vars)
		}
		return out
	default:
		return value
	}
}

// jsonEqual compares two decoded JSON values structurally, normalizing the numeric and
// slice-type mismatches between freshly-decoded corpus values (float64, []any) and the
// server's marshaled event values. Both are round-tripped through encoding/json so an
// int field and a float64 corpus literal, or a []string and a []any, compare equal.
func jsonEqual(a, b any) bool {
	na, err := normalizeJSON(a)
	if err != nil {
		return false
	}
	nb, err := normalizeJSON(b)
	if err != nil {
		return false
	}
	return reflect.DeepEqual(na, nb)
}

func normalizeJSON(v any) (any, error) {
	raw, err := json.Marshal(v)
	if err != nil {
		return nil, err
	}
	var out any
	if err := json.Unmarshal(raw, &out); err != nil {
		return nil, err
	}
	return out, nil
}

// asInt coerces a JSON-decoded status field (float64) or a native int to int.
func asInt(v any) (int, bool) {
	switch n := v.(type) {
	case float64:
		return int(n), true
	case int:
		return n, true
	case int64:
		return int(n), true
	default:
		return 0, false
	}
}

// TestScenarioParity runs every spec/conformance/scenarios/*.json through the Go
// server as a subtest, asserting the normalized outbound event stream matches.
func TestScenarioParity(t *testing.T) {
	dir := scenariosDir(t)
	paths, err := filepath.Glob(filepath.Join(dir, "*.json"))
	if err != nil {
		t.Fatalf("glob scenarios: %v", err)
	}
	if len(paths) == 0 {
		t.Fatalf("no scenarios found in %s", dir)
	}

	for _, path := range paths {
		path := path
		name := strings.TrimSuffix(filepath.Base(path), ".json")
		t.Run(name, func(t *testing.T) {
			runScenario(t, path)
		})
	}
}

func runScenario(t *testing.T, path string) {
	t.Helper()
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read scenario: %v", err)
	}
	var sc scenario
	if err := json.Unmarshal(raw, &sc); err != nil {
		t.Fatalf("parse scenario: %v", err)
	}

	mock := buildMock(t, sc.MockLlmScript)
	tools := buildTools(sc.Server.Tools)
	knowledge := buildKnowledge(sc.Server.Knowledge)

	ls, err := SpawnLocal(
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(mock),
		WithLocalServerOption(WithTools(tools)),
		WithLocalServerOption(WithKnowledge(knowledge)),
		WithLocalServerOption(WithConfirmTools(sc.Server.ConfirmTools)),
	)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer ls.Shutdown()

	// Drive the server over the raw WebSocket transport (not the typed client) so the
	// runner asserts the exact wire frames, matching the Python reference's raw-frame
	// approach.
	transport := protocol.NewWebSocketTransport(ls.WSURL(), nil)
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if err := transport.Connect(ctx); err != nil {
		t.Fatalf("connect transport: %v", err)
	}
	defer transport.Close()

	vars := map[string]any{}
	for i, step := range sc.Steps {
		frame := subst(step.Send, vars)
		payload, err := json.Marshal(frame)
		if err != nil {
			t.Fatalf("step %d: marshal send: %v", i, err)
		}
		if err := transport.Send(payload); err != nil {
			t.Fatalf("step %d: send: %v", i, err)
		}
		matchExpected(t, transport, step.Expect, vars)
	}
}

// nextEvent returns the next protocol event, skipping non-semantic keepalive/pong
// frames (as the Python reference does).
func nextEvent(t *testing.T, transport protocol.Transport) map[string]any {
	t.Helper()
	for {
		select {
		case data, ok := <-transport.Receive():
			if !ok {
				if err := transport.Err(); err != nil {
					t.Fatalf("transport closed with error: %v", err)
				}
				t.Fatalf("transport closed before expected event")
			}
			var ev map[string]any
			if err := json.Unmarshal(data, &ev); err != nil {
				t.Fatalf("decode event: %v (raw=%s)", err, data)
			}
			if typ, _ := ev["type"].(string); typ == "keepalive" || typ == "pong" {
				continue
			}
			return ev
		case <-time.After(10 * time.Second):
			t.Fatalf("timed out waiting for next event")
			return nil
		}
	}
}

// matchExpected matches the outbound event stream against an ordered list of matchers,
// a faithful port of the Python reference's _match_expected state machine: one-event
// lookahead for `repeat` overrun, status / statusGte / assert checks, var capture, and
// accumulate + assertAccumulated.
func matchExpected(t *testing.T, transport protocol.Transport, matchers []matcher, vars map[string]any) {
	t.Helper()
	var pending map[string]any // one-event lookahead when a `repeat` matcher overruns
	for _, m := range matchers {
		accumulated := ""
		for {
			event := pending
			if event == nil {
				event = nextEvent(t, transport)
			}
			pending = nil

			eventType, _ := event["type"].(string)
			if m.Repeat && eventType != m.Type {
				// The repeated run ended; this event belongs to the next matcher.
				pending = event
				break
			}
			if eventType != m.Type {
				t.Fatalf("expected event type %q, got %q (event=%s)", m.Type, eventType, mustJSON(event))
			}

			if m.Status != nil {
				got, ok := asInt(event["status"])
				if !ok || got != *m.Status {
					t.Fatalf("%s: status %v != %d (event=%s)", m.Type, event["status"], *m.Status, mustJSON(event))
				}
			}
			if m.StatusGte != nil {
				got, ok := asInt(event["status"])
				if !ok || got < *m.StatusGte {
					t.Fatalf("%s: status %v < %d (event=%s)", m.Type, event["status"], *m.StatusGte, mustJSON(event))
				}
			}
			for path, expected := range m.Assert {
				got, ok := dot(t, event, path)
				if !ok {
					t.Fatalf("%s: assert path %q not present (event=%s)", m.Type, path, mustJSON(event))
				}
				if !jsonEqual(got, expected) {
					t.Fatalf("%s: %s = %s != %s (event=%s)", m.Type, path, mustJSON(got), mustJSON(expected), mustJSON(event))
				}
			}
			for varName, path := range m.Capture {
				got, ok := dot(t, event, path)
				if !ok {
					t.Fatalf("%s: capture path %q not present (event=%s)", m.Type, path, mustJSON(event))
				}
				vars[varName] = got
			}
			if m.Accumulate != "" {
				s, ok := event[m.Accumulate].(string)
				if !ok {
					t.Fatalf("%s: accumulate field %q not a string (event=%s)", m.Type, m.Accumulate, mustJSON(event))
				}
				accumulated += s
			}
			if !m.Repeat {
				break
			}
		}
		if m.AssertAccumulated != nil {
			if accumulated != *m.AssertAccumulated {
				t.Fatalf("%s: accumulated %q != %q", m.Type, accumulated, *m.AssertAccumulated)
			}
		}
	}
}

func mustJSON(v any) string {
	raw, err := json.Marshal(v)
	if err != nil {
		return fmt.Sprintf("%v", v)
	}
	return string(raw)
}
