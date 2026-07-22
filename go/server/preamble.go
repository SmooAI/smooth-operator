package server

import (
	"context"
	"log/slog"
	"os"
	"strings"
	"sync/atomic"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// Pearl th-9e9bfe — the optional fast-model preamble.
//
// When SMOOTH_AGENT_PREAMBLE_MODEL is set, a small fast model runs in PARALLEL with the
// main turn and streams ONE ephemeral "what I'm about to do" sentence (stream_preamble),
// covering the reasoning model's time-to-first-token. Unset/empty/whitespace ⇒ off, so the
// default behavior — and the number of model calls — is unchanged. Port of the Rust
// reference (rust/smooth-operator-server/src/runner.rs).
//
// It is best-effort and ephemeral: any failure is logged at debug and swallowed (never an
// error event, never a failed turn), the text is NEVER persisted, NEVER folded into the
// assistant reply, and NEVER present in eventual_response. If the real answer has already
// begun streaming, the preamble is dropped rather than popping in late.

// preambleMaxTokens caps the preamble call — one short sentence. Matches the Rust
// PREAMBLE_MAX_TOKENS.
const preambleMaxTokens = 64

// preambleSystemPrompt is byte-identical to the Rust PREAMBLE_SYSTEM_PROMPT so every
// server produces the same behavior from the same model.
const preambleSystemPrompt = `You are the assistant's voice while it works. In ONE short present-tense sentence (max ~12 words), tell the user what you're about to do to help with their message. Do NOT answer the question, do NOT greet, do NOT promise a specific result or outcome. Example: "Let me pull up your recent conversations." Reply with only that sentence — no quotes, no preamble, no markdown.`

// preambleModel reads the configured preamble model. Unset, empty, or whitespace-only ⇒
// "" ⇒ the feature is off and no extra model call is made.
func preambleModel() string {
	return strings.TrimSpace(os.Getenv("SMOOTH_AGENT_PREAMBLE_MODEL"))
}

// runPreamble generates and emits the turn's preamble. Intended to run on its own
// goroutine concurrently with the agent loop — it never gates the real turn.
//
// The client is the turn's own chat client (same gateway, same key); only the model id and
// the output cap are overridden, mirroring the Rust "clone the LLM config" construction.
// The user's message is the only user-role content — the preamble is generated WITHOUT any
// tool results, which is why it must never be mistaken for the answer.
//
// answerStarted is the shared first-answer-token guard: it is checked immediately before
// emitting so a slow preamble can never land after the real answer has begun.
func runPreamble(ctx context.Context, client core.ChatClient, model, requestID, userMessage string, answerStarted *atomic.Bool, sink EventSink) {
	resp, err := client.Chat(ctx, core.ChatRequest{
		Model: model,
		Messages: []core.ChatMessage{
			{Role: "system", Content: preambleSystemPrompt},
			{Role: "user", Content: userMessage},
		},
		MaxTokens: preambleMaxTokens,
	})
	if err != nil {
		// Best-effort: a failed/slow/cancelled preamble must never surface or block.
		slog.Debug("preamble generation failed (ignored)", "error", err)
		return
	}
	text := strings.TrimSpace(resp.Content)
	if text == "" || answerStarted.Load() {
		return
	}
	sink(streamPreamble(requestID, text))
}
