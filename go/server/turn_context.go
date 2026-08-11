package server

import (
	"context"
	"sync"

	"github.com/SmooAI/smooth-operator/go/protocol"
)

// TurnContext is the per-turn host context a tool sees during a send_message turn —
// the Go analog of the Rust ToolProviderContext's file-transfer fields
// (rust/smooth-operator/src/tool_provider.rs). It carries the turn's image and file
// attachments (so a host tool can read them) and a directive sink (where a host tool
// writes a client-side directive that lands on the turn's eventual_response). A turn
// with no attachments and no directive written leaves behavior byte-for-byte unchanged.
//
// The Go engine core dispatches every tool with the turn's context.Context (see
// SmoothAgent.dispatchTool → Tool.Execute), so the server attaches this onto the turn's
// context and a host tool retrieves it with TurnContextFrom(ctx).
type TurnContext struct {
	// Images the turn carried (multimodal turns). A host tool may read them; empty for
	// the text-only common case.
	//
	// NOTE (parity gap, documented): the Rust reference ALSO attaches images to the
	// engine's user message as OpenAI image_url content parts (via core
	// `with_user_images`). The pinned Go engine core
	// (smooth-operator-core/go) has no multimodal ChatMessage content or user-images
	// option — `ChatMessage.Content` is a plain string that openai.go serializes as a
	// string — so images cannot reach the model until that core ships multimodal
	// support and the pin is bumped. Until then images are surfaced to host tools only.
	Images []protocol.RequestImagesElem
	// Files the turn carried. NEVER sent to the model — a host tool reads these to
	// persist them into the agent's workspace, where ordinary tools (read_file, bash)
	// can then use them.
	Files []protocol.RequestFilesElem

	mu           sync.Mutex
	directive    any
	hasDirective bool
}

// SetDirective records a client-side directive for this turn (last-write-wins). A host
// tool calls TurnContextFrom(ctx).SetDirective(...) during Execute; the value is opaque
// (mirrors the Rust directive sink's serde_json::Value) and is emitted on the terminal
// eventual_response. Safe under ParallelToolCalls (concurrent tool dispatch).
func (t *TurnContext) SetDirective(v any) {
	t.mu.Lock()
	t.directive = v
	t.hasDirective = true
	t.mu.Unlock()
}

// Directive returns (value, true) when a host tool wrote a directive this turn, or
// (nil, false) when none was written — in which case eventual_response omits the
// `directive` field (back-compat), mirroring the Rust drain's Null check.
func (t *TurnContext) Directive() (any, bool) {
	t.mu.Lock()
	defer t.mu.Unlock()
	return t.directive, t.hasDirective
}

type turnContextKey struct{}

// withTurnContext attaches tc to ctx so tools dispatched on this ctx can read it.
func withTurnContext(ctx context.Context, tc *TurnContext) context.Context {
	return context.WithValue(ctx, turnContextKey{}, tc)
}

// TurnContextFrom returns the per-turn TurnContext a host tool may read, or nil when
// the turn carried none (e.g. any non-send_message code path).
func TurnContextFrom(ctx context.Context) *TurnContext {
	tc, _ := ctx.Value(turnContextKey{}).(*TurnContext)
	return tc
}
