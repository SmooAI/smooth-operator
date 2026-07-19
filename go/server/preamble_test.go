package server

import (
	"context"
	"errors"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// Tests for the optional fast-model preamble (pearl th-9e9bfe). The feature is mostly
// defined by what must NOT happen — no extra model call when off, no event after the
// answer starts, no error surfaced, nothing persisted — so the negatives carry the weight.

const testPreambleModel = "fast-preamble-model"

// scriptedClient is a core.ChatClient that records every call's model and delegates to a
// per-test handler, so a test can distinguish the preamble call (req.Model ==
// testPreambleModel) from the agent-loop call and block either one deterministically.
type scriptedClient struct {
	mu     sync.Mutex
	models []string
	handle func(ctx context.Context, req core.ChatRequest) (core.ChatResponse, error)
}

func (c *scriptedClient) Chat(ctx context.Context, req core.ChatRequest) (core.ChatResponse, error) {
	c.mu.Lock()
	c.models = append(c.models, req.Model)
	c.mu.Unlock()
	return c.handle(ctx, req)
}

// ChatStream satisfies core.StreamingChatClient (what the agent loop requires) by running
// the same handler and delivering its reply as a single content delta — enough for these
// tests, which care about ordering and event shape, not chunking.
func (c *scriptedClient) ChatStream(ctx context.Context, req core.ChatRequest) (<-chan core.ChatChunk, error) {
	resp, err := c.Chat(ctx, req)
	if err != nil {
		return nil, err
	}
	out := make(chan core.ChatChunk, 1)
	out <- core.ChatChunk{ContentDelta: resp.Content}
	close(out)
	return out, nil
}

// calledWithModel reports whether any recorded call used the given model.
func (c *scriptedClient) calledWithModel(model string) bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	for _, m := range c.models {
		if m == model {
			return true
		}
	}
	return false
}

// recordingSink collects emitted events and closes a channel the first time a
// stream_preamble lands, so a test can synchronize on the emission instead of sleeping.
type recordingSink struct {
	mu       sync.Mutex
	events   []map[string]any
	preamble chan struct{}
	once     sync.Once
}

func newRecordingSink() *recordingSink {
	return &recordingSink{preamble: make(chan struct{})}
}

func (s *recordingSink) sink(ev map[string]any) {
	s.mu.Lock()
	s.events = append(s.events, ev)
	s.mu.Unlock()
	if ev["type"] == "stream_preamble" {
		s.once.Do(func() { close(s.preamble) })
	}
}

// ofType returns every recorded event with the given type.
func (s *recordingSink) ofType(t string) []map[string]any {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []map[string]any
	for _, ev := range s.events {
		if ev["type"] == t {
			out = append(out, ev)
		}
	}
	return out
}

// runTurnWith drives one turn against an in-memory store with the given client, returning
// the result plus the sink that recorded the streamed frames.
func runTurnWith(t *testing.T, client core.ChatClient, userMessage string) (TurnResult, *recordingSink, SessionStore, string) {
	t.Helper()
	store := NewInMemorySessionStore()
	session, err := store.CreateSession(context.Background(), "agent-1", "Alice", "alice@example.com")
	if err != nil {
		t.Fatalf("create session: %v", err)
	}
	sink := newRecordingSink()
	runner := NewTurnRunner(client, store, "", nil, nil, nil, nil, nil, "", "", nil)
	result, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", userMessage, sink.sink)
	if err != nil {
		t.Fatalf("run turn: %v", err)
	}
	return result, sink, store, session.ConversationID
}

// TestPreambleOffByDefault asserts the feature is inert unless the env var carries a real
// model: no stream_preamble event AND — the expensive half — no extra model call at all.
func TestPreambleOffByDefault(t *testing.T) {
	for name, value := range map[string]string{"unset": "", "whitespace": "   \t "} {
		t.Run(name, func(t *testing.T) {
			t.Setenv("SMOOTH_AGENT_PREAMBLE_MODEL", value)
			client := &scriptedClient{handle: func(_ context.Context, req core.ChatRequest) (core.ChatResponse, error) {
				if req.Model == testPreambleModel {
					t.Errorf("preamble model was called with the feature off")
				}
				return core.ChatResponse{Content: "Our return window is 30 days."}, nil
			}}

			_, sink, _, _ := runTurnWith(t, client, "what is the return policy?")

			if got := sink.ofType("stream_preamble"); len(got) != 0 {
				t.Fatalf("expected no stream_preamble events with the feature off, got %d", len(got))
			}
			// The turn must make exactly the calls it made before the feature existed —
			// one agent-loop call, no preamble call.
			client.mu.Lock()
			calls := len(client.models)
			client.mu.Unlock()
			if calls != 1 {
				t.Fatalf("expected exactly 1 model call (the turn) with the feature off, got %d", calls)
			}
		})
	}
}

// TestPreambleEmitsDocumentedShape asserts the emitted frame matches
// spec/events/stream-preamble.schema.json exactly — token duplicated at the top level and
// under data, requestId echoed in both, an epoch-ms timestamp, and no extra keys.
//
// Deterministic ordering: the agent-loop call blocks until the preamble has been emitted
// (observed via the sink), so the answer can never win the race in this test.
func TestPreambleEmitsDocumentedShape(t *testing.T) {
	t.Setenv("SMOOTH_AGENT_PREAMBLE_MODEL", testPreambleModel)
	sink := newRecordingSink()
	client := &scriptedClient{handle: func(_ context.Context, req core.ChatRequest) (core.ChatResponse, error) {
		if req.Model == testPreambleModel {
			return core.ChatResponse{Content: "  Let me pull up your return policy.  "}, nil
		}
		// The main turn waits for the preamble to land first.
		select {
		case <-sink.preamble:
		case <-time.After(5 * time.Second):
			return core.ChatResponse{}, errors.New("timed out waiting for the preamble")
		}
		return core.ChatResponse{Content: "Our return window is 30 days."}, nil
	}}

	store := NewInMemorySessionStore()
	session, err := store.CreateSession(context.Background(), "agent-1", "Alice", "alice@example.com")
	if err != nil {
		t.Fatalf("create session: %v", err)
	}
	runner := NewTurnRunner(client, store, "", nil, nil, nil, nil, nil, "", "", nil)
	if _, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "req-42", "what is the return policy?", sink.sink); err != nil {
		t.Fatalf("run turn: %v", err)
	}

	events := sink.ofType("stream_preamble")
	if len(events) != 1 {
		t.Fatalf("expected exactly 1 stream_preamble event, got %d", len(events))
	}
	ev := events[0]
	if len(ev) != 5 {
		t.Fatalf("stream_preamble has %d keys, want 5 (type, requestId, token, data, timestamp): %+v", len(ev), ev)
	}
	if ev["requestId"] != "req-42" {
		t.Errorf("requestId = %v, want req-42", ev["requestId"])
	}
	// The reply is trimmed before emission (matching Rust's resp.content.trim()).
	if ev["token"] != "Let me pull up your return policy." {
		t.Errorf("token = %q, want the trimmed preamble sentence", ev["token"])
	}
	data, ok := ev["data"].(map[string]any)
	if !ok {
		t.Fatalf("data is %T, want map[string]any", ev["data"])
	}
	if len(data) != 2 || data["requestId"] != "req-42" || data["token"] != ev["token"] {
		t.Errorf("data = %+v, want {requestId: req-42, token: <same as top-level>}", data)
	}
	ts, ok := ev["timestamp"].(int64)
	if !ok || ts <= 0 {
		t.Errorf("timestamp = %v (%T), want a positive epoch-ms int64", ev["timestamp"], ev["timestamp"])
	}
	// The preamble call is capped and carries only the system prompt + the user message.
	if !client.calledWithModel(testPreambleModel) {
		t.Error("expected the preamble model to be called")
	}
}

// TestPreamblePassesPromptAndCapVerbatim asserts the preamble call is built the way the
// Rust reference builds it: the verbatim system prompt, the user's message as the ONLY
// user-role content (no tool results), and a 64-token output cap.
func TestPreamblePassesPromptAndCapVerbatim(t *testing.T) {
	var got core.ChatRequest
	client := &scriptedClient{handle: func(_ context.Context, req core.ChatRequest) (core.ChatResponse, error) {
		got = req
		return core.ChatResponse{Content: "Checking that now."}, nil
	}}
	var answerStarted atomic.Bool
	sink := newRecordingSink()

	runPreamble(context.Background(), client, testPreambleModel, "r-1", "where is my order?", &answerStarted, sink.sink)

	if got.Model != testPreambleModel {
		t.Errorf("model = %q, want the configured preamble model", got.Model)
	}
	if got.MaxTokens != 64 {
		t.Errorf("maxTokens = %d, want 64", got.MaxTokens)
	}
	if len(got.Messages) != 2 {
		t.Fatalf("expected exactly 2 messages (system + user), got %d", len(got.Messages))
	}
	if got.Messages[0].Role != "system" || got.Messages[0].Content != preambleSystemPrompt {
		t.Errorf("message[0] = %+v, want the verbatim system prompt", got.Messages[0])
	}
	if got.Messages[1].Role != "user" || got.Messages[1].Content != "where is my order?" {
		t.Errorf("message[1] = %+v, want the user's message", got.Messages[1])
	}
	if len(got.Tools) != 0 {
		t.Errorf("expected no tools on the preamble call, got %d", len(got.Tools))
	}
}

// TestPreambleSuppressedOnceAnswerStarted is the race guard, made deterministic with
// channels rather than sleeps: the preamble call is HELD, the test flips the shared
// answer-started flag while it is held (exactly what the turn's stream loop does on the
// first real token), then releases it. runPreamble is awaited to completion before
// asserting, so "no event" is a settled fact, not a timing window.
func TestPreambleSuppressedOnceAnswerStarted(t *testing.T) {
	held := make(chan struct{})    // closed when the preamble call is in flight
	release := make(chan struct{}) // closed to let the preamble call return
	client := &scriptedClient{handle: func(_ context.Context, _ core.ChatRequest) (core.ChatResponse, error) {
		close(held)
		<-release
		return core.ChatResponse{Content: "Let me look that up."}, nil
	}}
	var answerStarted atomic.Bool
	sink := newRecordingSink()

	done := make(chan struct{})
	go func() {
		defer close(done)
		runPreamble(context.Background(), client, testPreambleModel, "r-1", "hello", &answerStarted, sink.sink)
	}()

	<-held                    // the preamble is mid-call...
	answerStarted.Store(true) // ...and the real answer starts streaming
	close(release)            // now let the preamble resolve
	<-done                    // and wait for it to finish deciding

	if got := sink.ofType("stream_preamble"); len(got) != 0 {
		t.Fatalf("preamble emitted %d event(s) after the answer had started; want none", len(got))
	}
}

// TestPreambleEmptyReplySuppressed asserts a model that returns nothing usable emits
// nothing (no empty-token frame).
func TestPreambleEmptyReplySuppressed(t *testing.T) {
	client := &scriptedClient{handle: func(_ context.Context, _ core.ChatRequest) (core.ChatResponse, error) {
		return core.ChatResponse{Content: "   \n "}, nil
	}}
	var answerStarted atomic.Bool
	sink := newRecordingSink()

	runPreamble(context.Background(), client, testPreambleModel, "r-1", "hello", &answerStarted, sink.sink)

	if got := sink.ofType("stream_preamble"); len(got) != 0 {
		t.Fatalf("expected no event for an empty preamble reply, got %d", len(got))
	}
}

// TestPreambleFailureNeverSurfaces asserts a failing preamble is swallowed: the turn
// completes normally, and NO error event (and no preamble event) reaches the client.
func TestPreambleFailureNeverSurfaces(t *testing.T) {
	t.Setenv("SMOOTH_AGENT_PREAMBLE_MODEL", testPreambleModel)
	failed := make(chan struct{})
	client := &scriptedClient{handle: func(_ context.Context, req core.ChatRequest) (core.ChatResponse, error) {
		if req.Model == testPreambleModel {
			close(failed)
			return core.ChatResponse{}, errors.New("preamble gateway exploded")
		}
		// Keep the turn open until the preamble has definitively failed, so the
		// assertions below observe the settled state rather than a race.
		select {
		case <-failed:
		case <-time.After(5 * time.Second):
			t.Error("timed out waiting for the preamble call")
		}
		return core.ChatResponse{Content: "Our return window is 30 days."}, nil
	}}

	result, sink, _, _ := runTurnWith(t, client, "what is the return policy?")

	if result.Reply != "Our return window is 30 days." {
		t.Errorf("reply = %q, want the turn to complete normally", result.Reply)
	}
	if got := sink.ofType("error"); len(got) != 0 {
		t.Fatalf("a preamble failure surfaced %d error event(s); want none", len(got))
	}
	if got := sink.ofType("stream_preamble"); len(got) != 0 {
		t.Fatalf("a failed preamble emitted %d event(s); want none", len(got))
	}
}

// TestPreambleIsEphemeral asserts the preamble text is never folded into the turn's
// answer: it is absent from TurnResult.Reply (what eventual_response is built from) and
// from the persisted conversation messages.
func TestPreambleIsEphemeral(t *testing.T) {
	t.Setenv("SMOOTH_AGENT_PREAMBLE_MODEL", testPreambleModel)
	const preambleText = "Let me pull up your return policy."
	sink := newRecordingSink()
	client := &scriptedClient{handle: func(_ context.Context, req core.ChatRequest) (core.ChatResponse, error) {
		if req.Model == testPreambleModel {
			return core.ChatResponse{Content: preambleText}, nil
		}
		select {
		case <-sink.preamble:
		case <-time.After(5 * time.Second):
			return core.ChatResponse{}, errors.New("timed out waiting for the preamble")
		}
		return core.ChatResponse{Content: "Our return window is 30 days."}, nil
	}}

	store := NewInMemorySessionStore()
	session, err := store.CreateSession(context.Background(), "agent-1", "Alice", "alice@example.com")
	if err != nil {
		t.Fatalf("create session: %v", err)
	}
	runner := NewTurnRunner(client, store, "", nil, nil, nil, nil, nil, "", "", nil)
	result, err := runner.Run(context.Background(), session.SessionID, session.ConversationID, "r-1", "what is the return policy?", sink.sink)
	if err != nil {
		t.Fatalf("run turn: %v", err)
	}

	if len(sink.ofType("stream_preamble")) != 1 {
		t.Fatal("expected the preamble to have been emitted for this test to be meaningful")
	}
	if strings.Contains(result.Reply, preambleText) {
		t.Errorf("reply %q contains the preamble text; it must never be folded into the answer", result.Reply)
	}
	messages, err := store.ListMessages(context.Background(), session.ConversationID, maxPriorMessages)
	if err != nil {
		t.Fatalf("list messages: %v", err)
	}
	for _, m := range messages {
		if strings.Contains(m.Text, preambleText) {
			t.Errorf("persisted message %q contains the preamble text; it must never be persisted", m.Text)
		}
	}
}
