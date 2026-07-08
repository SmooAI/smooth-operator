package server

import (
	"context"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// runTurn drives one TurnRunner turn end-to-end against an in-memory store + a mock
// engine scripted with reply, returning the settled TurnResult. The sink is discarded —
// these tests assert the returned citations, not the streamed frames.
func runTurn(t *testing.T, knowledge core.Knowledge, reply, userMessage string) TurnResult {
	t.Helper()
	store := NewInMemorySessionStore()
	session, err := store.CreateSession(context.Background(), "agent-1", "Alice", "alice@example.com")
	if err != nil {
		t.Fatalf("create session: %v", err)
	}
	mock := core.NewMockLlmProvider().PushText(reply)
	runner := NewTurnRunner(mock, store, "", knowledge, nil, nil, nil, nil, "", "", nil)
	result, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", userMessage, func(map[string]any) {})
	if err != nil {
		t.Fatalf("run turn: %v", err)
	}
	return result
}

// TestRunPopulatesCitationsFromKnowledge asserts that when the runner is wired with a
// seeded knowledge base, a grounded turn's result carries a citation per retrieved hit
// with id/title ← source and snippet ← the chunk content (the deterministic fields the
// conformance corpus asserts — score is computed and not checked here).
func TestRunPopulatesCitationsFromKnowledge(t *testing.T) {
	kb := &core.InMemoryKnowledge{}
	kb.Ingest("SmooAI returns are accepted within 30 days of delivery for a full refund.", "returns.md")

	result := runTurn(t, kb, "Our return window is 30 days.", "what is the return policy?")

	if len(result.Citations) == 0 {
		t.Fatal("expected at least one citation from the seeded knowledge base, got none")
	}
	got := result.Citations[0]
	if got.ID != "returns.md" {
		t.Errorf("citation id = %q, want %q", got.ID, "returns.md")
	}
	if got.Title != "returns.md" {
		t.Errorf("citation title = %q, want %q", got.Title, "returns.md")
	}
	if got.Snippet != "SmooAI returns are accepted within 30 days of delivery for a full refund." {
		t.Errorf("citation snippet = %q, want the seeded content", got.Snippet)
	}
	// returns.md is not an http(s) source, so url must be omitted (empty → absent on the wire).
	if got.URL != "" {
		t.Errorf("citation url = %q, want empty for a non-http source", got.URL)
	}
}

// TestRunNoKnowledgeNoCitations asserts that a turn run with no knowledge retriever
// returns empty citations (so the terminal eventual_response omits data.data.citations,
// per the absent-when-empty wire contract).
func TestRunNoKnowledgeNoCitations(t *testing.T) {
	result := runTurn(t, nil, "Sorry, I don't know.", "what is the return policy?")
	if len(result.Citations) != 0 {
		t.Fatalf("expected no citations without a knowledge base, got %d: %+v", len(result.Citations), result.Citations)
	}
}

// TestRunCitationSetsURLForHTTPSource asserts an http(s) source surfaces as the citation
// url (matching the TS/Rust isUrl behavior), while id/title still carry the source.
func TestRunCitationSetsURLForHTTPSource(t *testing.T) {
	kb := &core.InMemoryKnowledge{}
	kb.Ingest("Shipping takes 5-7 business days.", "https://smoo.ai/shipping")

	result := runTurn(t, kb, "5-7 business days.", "how long does shipping take?")

	if len(result.Citations) == 0 {
		t.Fatal("expected a citation from the seeded knowledge base, got none")
	}
	got := result.Citations[0]
	if got.URL != "https://smoo.ai/shipping" {
		t.Errorf("citation url = %q, want the http source", got.URL)
	}
	if got.ID != "https://smoo.ai/shipping" || got.Title != "https://smoo.ai/shipping" {
		t.Errorf("citation id/title = %q/%q, want the source", got.ID, got.Title)
	}
}
