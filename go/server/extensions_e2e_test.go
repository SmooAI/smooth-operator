package server

// End-to-end SEP extension hosting through the REAL dispatcher/WS turn path: a
// scripted model calls an extension-registered tool (echo.say), the host spawns
// the extension subprocess, and the tool result flows back — asserting the ext
// tool composes with the SMOODEV-590 enabled_tools filtering exactly like a
// built-in tool. The extension subprocess is a self-re-exec of this test binary
// (see TestMain / runServerEchoPeer), so the test needs no separate binary or node.

import (
	"bufio"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

func TestMain(m *testing.M) {
	if os.Getenv("SEP_ECHO_PEER") == "1" {
		runServerEchoPeer()
		os.Exit(0)
	}
	os.Exit(m.Run())
}

// runServerEchoPeer is a minimal dependency-free SEP extension: it registers a
// `say` tool and echoes the phrase argument back. Enough to prove the server
// hosts a real extension through a turn.
func runServerEchoPeer() {
	write := func(v any) {
		b, _ := json.Marshal(v)
		_, _ = os.Stdout.Write(append(b, '\n'))
	}
	reply := func(id json.RawMessage, result any) {
		rb, _ := json.Marshal(result)
		write(map[string]any{"jsonrpc": "2.0", "id": id, "result": json.RawMessage(rb)})
	}
	r := bufio.NewReader(os.Stdin)
	for {
		line, err := r.ReadString('\n')
		if len(line) > 0 {
			var f struct {
				ID     json.RawMessage `json:"id"`
				Method string          `json:"method"`
				Params json.RawMessage `json:"params"`
			}
			if json.Unmarshal([]byte(line), &f) == nil {
				switch f.Method {
				case "initialize":
					reply(f.ID, map[string]any{
						"protocol_version": 1,
						"extension":        map[string]any{"name": "echo", "version": "0.1.0"},
						"registrations": map[string]any{
							"tools": []any{map[string]any{
								"name":        "say",
								"description": "Echo a phrase back.",
								"parameters":  map[string]any{"type": "object", "properties": map[string]any{"phrase": map[string]any{"type": "string"}}, "required": []string{"phrase"}},
							}},
						},
					})
				case "ping":
					reply(f.ID, map[string]any{})
				case "tool/execute":
					var p struct {
						Arguments struct {
							Phrase string `json:"phrase"`
						} `json:"arguments"`
					}
					_ = json.Unmarshal(f.Params, &p)
					reply(f.ID, map[string]any{"content": p.Arguments.Phrase})
				case "shutdown":
					reply(f.ID, map[string]any{})
					os.Exit(0)
				}
			}
		}
		if err != nil {
			return
		}
	}
}

// writeEchoExtension writes a temp extensions dir holding an echo/extension.toml
// that runs this test binary as the SEP echo peer, and points the host's discovery
// + allowlist env at it. Returns nothing — env + files are set for the test.
func writeEchoExtension(t *testing.T) {
	t.Helper()
	dir := t.TempDir()
	extDir := filepath.Join(dir, "echo")
	if err := os.MkdirAll(extDir, 0o755); err != nil {
		t.Fatal(err)
	}
	toml := "name = \"echo\"\nversion = \"0.1.0\"\n[run]\ncommand = \"" + os.Args[0] + "\"\n[run.env]\nSEP_ECHO_PEER = \"1\"\n[capabilities]\ntools = true\n"
	if err := os.WriteFile(filepath.Join(extDir, "extension.toml"), []byte(toml), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("SMOOTH_EXTENSIONS_DIR", dir)
	t.Setenv("SMOOTH_EXTENSIONS_ALLOW", "echo")
}

// extScenario drives one scripted turn that calls echo.say through the real
// dispatcher, returning the tool-result text the model saw. cfg (nil → full tool
// set) exercises the enabled_tools gate; allow controls the trust allowlist.
func extScenario(t *testing.T, cfg *AgentConfig, allow string) string {
	t.Helper()
	writeEchoExtension(t)
	if allow != "echo" {
		t.Setenv("SMOOTH_EXTENSIONS_ALLOW", allow)
	}

	mock := core.NewMockLlmProvider()
	mock.PushToolCall("call-1", "echo.say", `{"phrase":"hi there"}`)
	mock.PushText("Done.")

	opts := []LocalOption{
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(mock),
	}
	if cfg != nil {
		opts = append(opts, WithLocalServerOption(WithAgentConfigResolver(NewStaticAgentConfigResolver(map[string]*AgentConfig{e2eAgentID: cfg}))))
	}

	ls, err := SpawnLocal(opts...)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer ls.Shutdown()

	transport := connectTransport(t, ls)
	defer transport.Close()
	sessionID := createSession(t, transport)
	sendFrame(t, transport, map[string]any{"action": "send_message", "requestId": "r-msg", "sessionId": sessionID, "message": "say hi"})

	var toolResult string
	for {
		ev := nextEv(t, transport)
		typ, _ := ev["type"].(string)
		if typ == "stream_chunk" {
			if res, ok := dot(t, ev, "data.state.rawResponse.toolResult.result"); ok {
				if s, _ := res.(string); s != "" {
					toolResult = s
				}
			}
		}
		if typ == "eventual_response" {
			break
		}
	}
	return toolResult
}

// TestExtensionToolExecutesThroughRealTurn: an allowlisted extension's tool is
// discovered, spawned, registered, and executed through the real WS/dispatcher path.
func TestExtensionToolExecutesThroughRealTurn(t *testing.T) {
	if res := extScenario(t, nil, "echo"); res != "hi there" {
		t.Errorf("tool result = %q, want the echo output %q", res, "hi there")
	}
}

// TestExtensionToolRespectsEnabledToolsFilter: the ext tool composes with the
// per-agent enabled_tools gate — present when enabled, filtered when not.
func TestExtensionToolRespectsEnabledToolsFilter(t *testing.T) {
	t.Run("enabled → executes", func(t *testing.T) {
		cfg := &AgentConfig{EnabledTools: []EnabledTool{{ToolID: "echo.say", Enabled: true}}}
		if res := extScenario(t, cfg, "echo"); res != "hi there" {
			t.Errorf("tool result = %q, want %q", res, "hi there")
		}
	})
	t.Run("not enabled → filtered out", func(t *testing.T) {
		cfg := &AgentConfig{EnabledTools: []EnabledTool{{ToolID: "some_other_tool", Enabled: true}}}
		res := extScenario(t, cfg, "echo")
		if res == "hi there" {
			t.Error("echo.say should have been filtered out by enabled_tools")
		}
		if !strings.Contains(res, "unknown tool") {
			t.Errorf("filtered tool call result = %q, want an unknown-tool error", res)
		}
	})
}

// TestExtensionDefaultDenyNoAllowlist: with an empty allowlist no host is built,
// so the ext tool never exists — the model's call resolves to unknown tool.
func TestExtensionDefaultDenyNoAllowlist(t *testing.T) {
	res := extScenario(t, nil, "")
	if res == "hi there" {
		t.Error("default deny (empty allowlist) must not spawn the extension")
	}
	if !strings.Contains(res, "unknown tool") {
		t.Errorf("default-deny result = %q, want an unknown-tool error", res)
	}
}
