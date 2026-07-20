package server

import (
	"context"
	"testing"
	"time"
)

// getMessages drives one get_conversation_messages frame and returns its data payload.
func getMessages(t *testing.T, store SessionStore, frame map[string]any) map[string]any {
	t.Helper()
	sink, events := capture()
	frame["action"] = "get_conversation_messages"
	dispatchJSON(t, bareDispatcher(store), frame, sink)
	if len(*events) != 1 {
		t.Fatalf("want 1 event, got %d: %+v", len(*events), *events)
	}
	ev := (*events)[0]
	data, ok := ev["data"].(map[string]any)
	if !ok {
		t.Fatalf("no data payload in %+v", ev)
	}
	return data
}

func TestGetConversationMessagesNewestFirst(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	_, _ = store.AppendMessage(ctx, s.ConversationID, Inbound, "first")
	_, _ = store.AppendMessage(ctx, s.ConversationID, Outbound, "second")

	data := getMessages(t, store, map[string]any{"requestId": "r1", "sessionId": s.SessionID})

	if data["hasMore"] != false {
		t.Errorf("hasMore = %v, want false", data["hasMore"])
	}
	msgs := data["messages"].([]map[string]any)
	if len(msgs) != 2 {
		t.Fatalf("want 2 messages, got %d", len(msgs))
	}
	// Newest-first: the outbound "second" leads.
	if msgs[0]["direction"] != "outbound" || msgs[1]["direction"] != "inbound" {
		t.Errorf("directions = %v / %v, want outbound / inbound (newest first)", msgs[0]["direction"], msgs[1]["direction"])
	}
	if got := msgs[0]["content"].(map[string]any)["text"]; got != "second" {
		t.Errorf("content.text = %v, want %q", got, "second")
	}
	if msgs[0]["id"] == "" || msgs[0]["id"] == nil {
		t.Error("message id missing")
	}
	created, ok := msgs[0]["createdAt"].(string)
	if !ok {
		t.Fatalf("createdAt not a string: %v", msgs[0]["createdAt"])
	}
	if _, err := time.Parse(time.RFC3339, created); err != nil {
		t.Errorf("createdAt %q is not RFC3339: %v", created, err)
	}
}

func TestGetConversationMessagesUnknownSession(t *testing.T) {
	sink, events := capture()
	dispatchJSON(t, bareDispatcher(NewInMemorySessionStore()),
		map[string]any{"action": "get_conversation_messages", "requestId": "r1", "sessionId": "nope"}, sink)

	ev := singleEvent(t, events, "error")
	if code := errorCode(t, ev); code != "SESSION_NOT_FOUND" {
		t.Errorf("error code = %q, want SESSION_NOT_FOUND", code)
	}
}

func TestGetConversationMessagesMissingSessionID(t *testing.T) {
	sink, events := capture()
	dispatchJSON(t, bareDispatcher(NewInMemorySessionStore()),
		map[string]any{"action": "get_conversation_messages", "requestId": "r1"}, sink)

	ev := singleEvent(t, events, "error")
	if code := errorCode(t, ev); code != "VALIDATION_ERROR" {
		t.Errorf("error code = %q, want VALIDATION_ERROR", code)
	}
}

func TestGetConversationMessagesLimitAndHasMore(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	for _, text := range []string{"m1", "m2", "m3", "m4"} {
		_, _ = store.AppendMessage(ctx, s.ConversationID, Inbound, text)
	}

	data := getMessages(t, store, map[string]any{"requestId": "r1", "sessionId": s.SessionID, "limit": 2})

	msgs := data["messages"].([]map[string]any)
	if len(msgs) != 2 {
		t.Fatalf("limit=2 not honored, got %d", len(msgs))
	}
	if data["hasMore"] != true {
		t.Errorf("hasMore = %v, want true", data["hasMore"])
	}
	if got := msgs[0]["content"].(map[string]any)["text"]; got != "m4" {
		t.Errorf("newest message = %v, want m4", got)
	}
}

// pageAll walks every page with limit=1, following nextCursor, and returns the message texts
// in the order they were served. Fails if paging doesn't terminate.
func pageAll(t *testing.T, store SessionStore, sessionID string) []string {
	t.Helper()
	var texts []string
	cursor := ""
	for i := 0; ; i++ {
		if i > 100 {
			t.Fatal("paging did not terminate")
		}
		frame := map[string]any{"requestId": "r", "sessionId": sessionID, "limit": 1}
		if cursor != "" {
			frame["cursor"] = cursor
		}
		page := getMessages(t, store, frame)

		msgs := page["messages"].([]map[string]any)
		for _, m := range msgs {
			texts = append(texts, m["content"].(map[string]any)["text"].(string))
		}

		hasMore, _ := page["hasMore"].(bool)
		next, isString := page["nextCursor"].(string)
		if hasMore != isString {
			t.Fatalf("page %d: hasMore = %v but nextCursor = %v — must be non-null iff hasMore", i, hasMore, page["nextCursor"])
		}
		if !hasMore {
			return texts
		}
		cursor = next
	}
}

func TestGetConversationMessagesCursorRoundTrip(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	for _, text := range []string{"m1", "m2", "m3", "m4"} {
		_, _ = store.AppendMessage(ctx, s.ConversationID, Inbound, text)
	}

	got := pageAll(t, store, s.SessionID)

	// Newest-first across page boundaries, each message exactly once.
	want := []string{"m4", "m3", "m2", "m1"}
	if len(got) != len(want) {
		t.Fatalf("paged %d messages (%v), want %d", len(got), got, len(want))
	}
	for i := range want {
		if got[i] != want[i] {
			t.Errorf("page %d message = %q, want %q (full: %v)", i, got[i], want[i], got)
		}
	}
}

// TestGetConversationMessagesCursorIdenticalTimestamps is the regression test for the bug the
// Go server actually shipped: two messages with the SAME CreatedAt. Any `created_at <` cursor
// either drops both or repeats one; an id cursor names exactly one row and cannot.
func TestGetConversationMessagesCursorIdenticalTimestamps(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	_, _ = store.AppendMessage(ctx, s.ConversationID, Inbound, "older")
	_, _ = store.AppendMessage(ctx, s.ConversationID, Outbound, "newer")

	// Byte-identical timestamps on both messages.
	same := time.Date(2026, 1, 1, 0, 0, 30, 400_000_000, time.UTC)
	store.messages[s.ConversationID][0].CreatedAt = same
	store.messages[s.ConversationID][1].CreatedAt = same

	got := pageAll(t, store, s.SessionID)

	want := []string{"newer", "older"}
	if len(got) != len(want) {
		t.Fatalf("paged %d messages (%v), want both messages exactly once", len(got), got)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Errorf("page %d message = %q, want %q", i, got[i], want[i])
		}
	}
}

func TestGetConversationMessagesNextCursorNamesOldestInPage(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	for _, text := range []string{"m1", "m2", "m3"} {
		_, _ = store.AppendMessage(ctx, s.ConversationID, Inbound, text)
	}

	page := getMessages(t, store, map[string]any{"requestId": "r1", "sessionId": s.SessionID, "limit": 2})

	msgs := page["messages"].([]map[string]any)
	if len(msgs) != 2 {
		t.Fatalf("want 2 messages, got %d", len(msgs))
	}
	if page["nextCursor"] != msgs[len(msgs)-1]["id"] {
		t.Errorf("nextCursor = %v, want the oldest message in the page (%v)", page["nextCursor"], msgs[len(msgs)-1]["id"])
	}

	// Last page: hasMore false, nextCursor null.
	last := getMessages(t, store, map[string]any{"requestId": "r2", "sessionId": s.SessionID, "limit": 2, "cursor": page["nextCursor"]})
	if last["hasMore"] != false {
		t.Errorf("last page hasMore = %v, want false", last["hasMore"])
	}
	if last["nextCursor"] != nil {
		t.Errorf("last page nextCursor = %v, want nil", last["nextCursor"])
	}
}

func TestGetConversationMessagesUnknownCursor(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	_, _ = store.AppendMessage(ctx, s.ConversationID, Inbound, "m1")

	sink, events := capture()
	dispatchJSON(t, bareDispatcher(store),
		map[string]any{"action": "get_conversation_messages", "requestId": "r1", "sessionId": s.SessionID, "cursor": "no-such-id"}, sink)

	ev := singleEvent(t, events, "error")
	if code := errorCode(t, ev); code != "VALIDATION_ERROR" {
		t.Errorf("error code = %q, want VALIDATION_ERROR", code)
	}
}
