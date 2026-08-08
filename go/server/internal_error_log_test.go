package server

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"log/slog"
	"testing"
)

// failingSessionStore is an InMemorySessionStore whose GetSession always fails — the
// cheapest way to drive a handler into the INTERNAL_ERROR path. Embedding the interface
// means only the one method needs overriding.
type failingSessionStore struct {
	SessionStore
	err error
}

func (f *failingSessionStore) GetSession(ctx context.Context, sessionID string) (*StoredSession, error) {
	return nil, f.err
}

// TestInternalErrorIsLogged asserts the observability contract behind th-e7ef23: an
// INTERNAL_ERROR on the wire stays generic (no detail leaks to the client) but MUST leave
// the real cause in the host log. Before the fix every INTERNAL_ERROR site dropped the
// error on the floor, so a server failing every request logged nothing at all.
func TestInternalErrorIsLogged(t *testing.T) {
	var logs bytes.Buffer
	prev := slog.Default()
	slog.SetDefault(slog.New(slog.NewTextHandler(&logs, &slog.HandlerOptions{Level: slog.LevelError})))
	t.Cleanup(func() { slog.SetDefault(prev) })

	store := &failingSessionStore{SessionStore: NewInMemorySessionStore(), err: errors.New("gateway said 401")}
	d := NewFrameDispatcher(store, nil, AccessContext{}, "", nil, nil, nil, nil, nil, "", nil, nil, nil, nil)

	var events []map[string]any
	d.Dispatch(context.Background(), []byte(`{"action":"get_session","requestId":"r-9","sessionId":"s-1"}`), func(ev map[string]any) {
		events = append(events, ev)
	})

	if len(events) != 1 {
		t.Fatalf("expected 1 event, got %d", len(events))
	}
	raw, _ := json.Marshal(events[0])
	if !bytes.Contains(raw, []byte("INTERNAL_ERROR")) {
		t.Fatalf("expected INTERNAL_ERROR on the wire, got %s", raw)
	}
	if bytes.Contains(raw, []byte("gateway said 401")) {
		t.Fatalf("exception detail must NOT leak to the client: %s", raw)
	}

	logged := logs.String()
	for _, want := range []string{"INTERNAL_ERROR", "get_session", "r-9", "gateway said 401"} {
		if !bytes.Contains([]byte(logged), []byte(want)) {
			t.Fatalf("host log missing %q; got: %s", want, logged)
		}
	}
}
