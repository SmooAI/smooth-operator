package server

// SEP extension hosting for the Go operator server.
//
// Wires the engine's extension.ExtensionHost into a turn so a server-side agent
// can host extensions: discover extension.toml extensions, spawn them as
// JSON-RPC/ndjson subprocesses, and register their tools into the turn's tool
// set (composing with the SMOODEV-590 enabled_tools / authLevel filtering). The
// host's tools are appended to the base tools before filterTools/gateTools, so an
// extension tool named <ext>.<tool> is subject to the same per-agent enabled-tools
// gate as any built-in tool.
//
// ## Trust — default deny
// The server has no interactive trust prompt (a multi-session server can't stop to
// ask a human). SMOOTH_EXTENSIONS_ALLOW (comma-separated extension names) IS the
// trust decision: empty (the default) ⇒ no extension is ever spawned and the host
// is never built, so behavior is byte-for-byte unchanged.
//
// ## ui/confirm → the existing confirmation frame
// confirmUIProvider projects an extension's ui/confirm onto the operator
// protocol's write_confirmation_required / confirm_tool_action frames — the same
// out-of-band bridge the native write-tool HumanGate uses: register a resolver in
// the per-connection ConfirmationRegistry, emit the frame, and park the extension's
// request until the client answers with confirm_tool_action (or the turn ends).
// Every other ui/* degrades headless (interactive → {cancelled}, render-only → {}).

import (
	"context"
	"encoding/json"
	"os"
	"strings"
	"sync"
	"time"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	extension "github.com/SmooAI/smooth-operator-core/go/core/extension"
)

// uiMode is the frontend mode announced to extensions at handshake. The servers
// front the chat-widget, whose confirm lands as a chat-native button frame.
const uiMode = "widget"

// uiConfirmTimeout is how long a parked ui/confirm waits for the client's
// confirm_tool_action before the bridge resolves it as cancelled. Matches the
// native write-tool confirmation window.
const uiConfirmTimeout = 300 * time.Second

// ExtensionTurn is a per-turn extension host plus the teardown the runner needs.
// The dispatcher builds it, appends its tools to the turn, and calls Close when the
// turn ends (dropping any parked ui/confirm and shutting the subprocesses down).
type ExtensionTurn struct {
	host      *extension.ExtensionHost
	done      chan struct{}
	closeOnce sync.Once
}

// Tools returns the host's extension tools as core.Tool values for the turn.
func (et *ExtensionTurn) Tools() []core.Tool {
	if et == nil {
		return nil
	}
	proxies := et.host.Tools()
	out := make([]core.Tool, 0, len(proxies))
	for _, t := range proxies {
		out = append(out, t)
	}
	return out
}

// Close unparks any hung ui/confirm and shuts down the extension subprocesses.
// Idempotent; safe to defer.
func (et *ExtensionTurn) Close(ctx context.Context) {
	if et == nil {
		return
	}
	et.closeOnce.Do(func() { close(et.done) })
	et.host.ShutdownAll(ctx)
}

// confirmUIProvider bridges ui/confirm onto the confirmation frame and degrades
// every other ui/* headless. Bound to ONE turn (its sink, request id, session).
type confirmUIProvider struct {
	extension.DefaultHostDelegate
	sink          EventSink
	requestID     string
	sessionID     string
	confirmations *ConfirmationRegistry
	done          chan struct{}
}

// UIRequest answers an extension's ui/request. confirm rides the confirmation
// frame; render-only kinds accept-and-drop; select/input cancel (no chat source).
func (p *confirmUIProvider) UIRequest(ext string, params json.RawMessage) (json.RawMessage, *extension.RpcError) {
	var pr struct {
		Kind   string `json:"kind"`
		Prompt string `json:"prompt"`
	}
	_ = json.Unmarshal(params, &pr)
	switch pr.Kind {
	case "confirm":
		prompt := pr.Prompt
		if prompt == "" {
			prompt = "Confirm this action?"
		}
		// Register a fresh resolver for this session so the next inbound
		// confirm_tool_action resumes THIS request, then emit the frame and park.
		verdict := p.confirmations.Register(p.sessionID)
		p.sink(writeConfirmationRequired(p.requestID, ext, prompt))
		select {
		case approved := <-verdict:
			if approved {
				return json.RawMessage(`{"confirmed":true}`), nil
			}
			return json.RawMessage(`{"confirmed":false}`), nil
		case <-time.After(uiConfirmTimeout):
			return json.RawMessage(`{"cancelled":true}`), nil
		case <-p.done:
			// Turn ended before the human answered — a dismissed dialog.
			return json.RawMessage(`{"cancelled":true}`), nil
		}
	case "notify", "set_status", "set_widget", "set_title":
		// Render-only kinds: accept and drop — no chat frame, nothing to await.
		return json.RawMessage(`{}`), nil
	default:
		// select/input need an answer we can't source from a confirm button.
		return json.RawMessage(`{"cancelled":true}`), nil
	}
}

// parseExtensionAllowlist parses SMOOTH_EXTENSIONS_ALLOW into a set of allowed
// extension names (comma-separated, trimmed, empties dropped). Absent/blank ⇒
// empty ⇒ deny all.
func parseExtensionAllowlist(raw string) []string {
	var out []string
	for _, part := range strings.Split(raw, ",") {
		if s := strings.TrimSpace(part); s != "" {
			out = append(out, s)
		}
	}
	return out
}

// buildExtensionHost discovers, trust-gates (allowlist), and loads the per-turn
// extension host for a session's turn. Returns nil — the host is never built, zero
// overhead — when the allowlist is empty (default deny) or no allowed extension
// loads. confirmations is the connection's registry, shared with the write-tool
// HumanGate so a confirm_tool_action resolves whichever is parked.
func buildExtensionHost(ctx context.Context, sessionID, requestID string, sink EventSink, confirmations *ConfirmationRegistry) *ExtensionTurn {
	allow := parseExtensionAllowlist(os.Getenv("SMOOTH_EXTENSIONS_ALLOW"))
	if len(allow) == 0 {
		return nil // default deny — never spawn anything.
	}
	allowSet := make(map[string]struct{}, len(allow))
	for _, a := range allow {
		allowSet[a] = struct{}{}
	}

	// SMOOTH_EXTENSIONS_DIR overrides the discovery dir; else the engine default.
	global := strings.TrimSpace(os.Getenv("SMOOTH_EXTENSIONS_DIR"))
	if global == "" {
		global = extension.DefaultGlobalDir()
	}
	// No per-session workspace; project-scoped discovery keys off the process cwd.
	project := ""
	root := ""
	if cwd, err := os.Getwd(); err == nil {
		project = extension.ProjectDir(cwd)
		root = cwd
	}
	discovered, _ := extension.Discover(global, project)

	var allowed []extension.DiscoveredExtension
	for _, ext := range discovered {
		if _, ok := allowSet[ext.Manifest.Name]; ok {
			allowed = append(allowed, ext)
		}
	}
	if len(allowed) == 0 {
		return nil
	}

	done := make(chan struct{})
	delegate := &confirmUIProvider{
		sink:          sink,
		requestID:     requestID,
		sessionID:     sessionID,
		confirmations: confirmations,
		done:          done,
	}
	host, _ := extension.Load(
		ctx,
		allowed,
		extension.HostInfo{Name: "smooth-operator-server", Version: "0"},
		// Allowlisted ⇒ trusted (the allowlist is the trust decision); project-scoped
		// extensions load because trusted is true.
		extension.WorkspaceInfo{Root: root, Trusted: true},
		uiMode,
		[]string{"confirm"},
		delegate,
	)
	if host.IsEmpty() {
		close(done)
		return nil
	}
	return &ExtensionTurn{host: host, done: done}
}
