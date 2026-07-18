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
	s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com")
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
	s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com")
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

func TestGetConversationMessagesBeforeCursor(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com")
	_, _ = store.AppendMessage(ctx, s.ConversationID, Inbound, "older")
	_, _ = store.AppendMessage(ctx, s.ConversationID, Outbound, "newer")

	// Stamp the two appends a minute apart: back-to-back time.Now() calls can land on the
	// same tick, which would make the cursor assertion flaky.
	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	store.messages[s.ConversationID][0].CreatedAt = base
	store.messages[s.ConversationID][1].CreatedAt = base.Add(time.Minute)

	cursor := base.Add(30 * time.Second).Format(time.RFC3339)
	data := getMessages(t, store, map[string]any{"requestId": "r1", "sessionId": s.SessionID, "before": cursor})

	msgs := data["messages"].([]map[string]any)
	if len(msgs) != 1 {
		t.Fatalf("want only the pre-cursor message, got %d: %+v", len(msgs), msgs)
	}
	if got := msgs[0]["content"].(map[string]any)["text"]; got != "older" {
		t.Errorf("message = %v, want older", got)
	}
}

func TestGetConversationMessagesInvalidBefore(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com")

	sink, events := capture()
	dispatchJSON(t, bareDispatcher(store),
		map[string]any{"action": "get_conversation_messages", "requestId": "r1", "sessionId": s.SessionID, "before": "not-a-date"}, sink)

	ev := singleEvent(t, events, "error")
	if code := errorCode(t, ev); code != "VALIDATION_ERROR" {
		t.Errorf("error code = %q, want VALIDATION_ERROR", code)
	}
}
