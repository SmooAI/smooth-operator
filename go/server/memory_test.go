package server

import (
	"encoding/json"
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// Durable auto-recall parity (th-ebe27d / Rust PR #330 — the StorageAdapter::memory_for_access
// seam, tested in rust/smooth-operator-server/tests/injection_seams.rs).
//
// The engine already knew how to recall; what was missing on this server was the host's way to say
// WHICH store, so every turn ran without auto-recall no matter what the deployment had. Tests are
// named after their Rust counterparts so a parity gap stays visible.
//
// The recall block's header text is deliberately NOT asserted: the five cores currently inject three
// different strings for it (th-ffaeae). The assertion is on the recalled CONTENT reaching the model,
// which is the behavior the seam exists for.

const recalledFact = "always add shows to the smoo-hub watchlist"

// allContentSeen flattens everything the model was sent this turn — the surface a recalled memory
// must show up in.
func allContentSeen(t *testing.T, mock *core.MockLlmProvider) string {
	t.Helper()
	var sb strings.Builder
	for _, call := range mock.Calls() {
		blob, err := json.Marshal(call.Messages)
		if err != nil {
			t.Fatalf("marshal messages: %v", err)
		}
		sb.Write(blob)
	}
	return sb.String()
}

func memoryWithEntry() core.Memory {
	m := &core.InMemoryMemory{}
	m.Remember(recalledFact)
	return m
}

// runMemoryTurn drives one plain text turn with the given provider installed the way the server
// installs it (post-construction, alongside hooks).
func runMemoryTurn(t *testing.T, provider MemoryProvider, message string) *core.MockLlmProvider {
	t.Helper()
	mock := core.NewMockLlmProvider().PushText("ok")
	d := NewFrameDispatcher(NewInMemorySessionStore(), mock, AccessContext{}, "BASE PROMPT", nil, nil, nil, nil, nil, "", nil, nil, nil, nil)
	d.memoryProvider = provider
	sid := createSessionForTest(t, d)

	sink := sendAndWait(d, `{"action":"send_message","requestId":"r-1","sessionId":"`+sid+`","message":"`+message+`"}`)
	if sink.find("eventual_response") == nil {
		t.Fatal("turn did not complete")
	}
	return mock
}

// ── rust: no_memory_means_no_recall_injection ────────────────────────────────

// Default: no provider ⇒ no auto-recall. Guards against the seam injecting when absent — an unopted
// deployment's turn must be byte-for-byte what it was before.
func TestNoMemoryMeansNoRecallInjection(t *testing.T) {
	mock := runMemoryTurn(t, nil, "add shows to my watchlist")
	if strings.Contains(allContentSeen(t, mock), recalledFact) {
		t.Error("a turn with no memory provider carried a recalled memory")
	}
}

// A provider returning nil is the same as no provider — the seam must not fabricate a store just
// because one was installed.
func TestProviderReturningNilMeansNoRecallInjection(t *testing.T) {
	mock := runMemoryTurn(t, NewStaticMemoryProvider(nil), "add shows to my watchlist")
	if strings.Contains(allContentSeen(t, mock), recalledFact) {
		t.Error("a declining provider still injected a recalled memory")
	}
}

// ── rust: attached_memory_is_auto_recalled_into_the_turn ─────────────────────

// With a store attached the engine recalls the entries relevant to the user's message and injects
// them into the turn — the seam that lights up Big Smooth's durable auto-recall.
func TestAttachedMemoryIsAutoRecalledIntoTheTurn(t *testing.T) {
	// The message shares "add", "shows", "watchlist" with the stored entry, so the engine's
	// word-overlap recall surfaces it.
	mock := runMemoryTurn(t, NewStaticMemoryProvider(memoryWithEntry()), "add shows to my watchlist")
	if !strings.Contains(allContentSeen(t, mock), recalledFact) {
		t.Errorf("an attached memory was not recalled into the turn: %s", allContentSeen(t, mock))
	}
}

// An unrelated message recalls nothing: relevance-gated by the engine, not a blanket dump of every
// stored memory into every turn. The message shares NO token with the entry — the bundled lexical
// scorer counts raw token overlap with no stopword filter, so a single shared "the" would be enough
// to score a hit.
func TestIrrelevantMessageRecallsNothing(t *testing.T) {
	mock := runMemoryTurn(t, NewStaticMemoryProvider(memoryWithEntry()), "explain quantum entanglement")
	if strings.Contains(allContentSeen(t, mock), recalledFact) {
		t.Error("an unrelated message recalled a memory it shares no token with")
	}
}

// The seam is access-scoped (mirroring knowledge) so a multi-tenant host can bind memory to the
// requester — the argument must actually reach the provider.
func TestProviderSeesTheCallersAccess(t *testing.T) {
	rec := &recordingMemoryProvider{}
	runMemoryTurn(t, rec, "hello")
	if rec.calls != 1 {
		t.Fatalf("provider called %d times, want exactly 1", rec.calls)
	}
}

type recordingMemoryProvider struct{ calls int }

func (r *recordingMemoryProvider) MemoryForAccess(_ AccessContext) core.Memory {
	r.calls++
	return nil
}
