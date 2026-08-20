package server

import (
	"context"
	"testing"
)

// bareDispatcher builds a dispatcher wired with just a store — enough to drive
// list_conversations / create_conversation_session resume directly.
func bareDispatcher(store SessionStore) *FrameDispatcher {
	return NewFrameDispatcher(store, nil, AccessContext{}, "", nil, nil, nil, nil, nil, "", nil, nil, nil, nil)
}

func TestConversationTitle(t *testing.T) {
	tests := []struct {
		name     string
		first    string
		fallback string
		want     string
	}{
		{"plain", "Hello there", "fb", "Hello there"},
		{"trims whitespace", "   spaced   ", "fb", "spaced"},
		{"strips leading heading", "### Big title", "fb", "Big title"},
		{"strips leading bullet", "- do the thing", "fb", "do the thing"},
		{"strips leading quote+emphasis", "> _quoted_ line", "fb", "quoted_ line"},
		{"strips leading control chars", "keep this", "fb", "keep this"},
		{"empty falls back to name", "", "My Conversation", "My Conversation"},
		{"markdown-only falls back", "###   ", "Fallback", "Fallback"},
		{
			"truncates long to 60 with ellipsis",
			"012345678901234567890123456789012345678901234567890123456789EXTRA",
			"fb",
			"012345678901234567890123456789012345678901234567890123456789…",
		},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			got := conversationTitle(tc.first, tc.fallback)
			if got != tc.want {
				t.Fatalf("conversationTitle(%q,%q) = %q, want %q", tc.first, tc.fallback, got, tc.want)
			}
			// The clipped case must be exactly 60 visible chars + the ellipsis rune.
			if r := []rune(got); tc.name == "truncates long to 60 with ellipsis" && len(r) != 61 {
				t.Fatalf("clipped title rune length = %d, want 61", len(r))
			}
		})
	}
}

func TestListConversationsFiltersEmpties(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()

	// A: empty conversation (created, never messaged) → excluded.
	if _, err := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true}); err != nil {
		t.Fatalf("create A: %v", err)
	}
	// B: has messages → included, title from first inbound.
	b, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	if _, err := store.AppendMessage(ctx, b.ConversationID, Inbound, "## First user line"); err != nil {
		t.Fatalf("append B in: %v", err)
	}
	if _, err := store.AppendMessage(ctx, b.ConversationID, Outbound, "agent reply"); err != nil {
		t.Fatalf("append B out: %v", err)
	}

	summaries, err := store.ListConversations(ctx, ConversationScope{Unscoped: true})
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(summaries) != 1 {
		t.Fatalf("want 1 non-empty conversation, got %d: %+v", len(summaries), summaries)
	}
	got := summaries[0]
	if got.ConversationID != b.ConversationID {
		t.Errorf("conversationId = %q, want %q", got.ConversationID, b.ConversationID)
	}
	if got.MessageCount != 2 {
		t.Errorf("messageCount = %d, want 2", got.MessageCount)
	}
	if got.FirstInbound != "## First user line" {
		t.Errorf("firstInbound = %q, want raw inbound text", got.FirstInbound)
	}
	if got.UpdatedAt.IsZero() {
		t.Error("updatedAt is zero; want the last append time")
	}
}

func TestListConversationsSortedMostRecentFirst(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()

	older, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	_, _ = store.AppendMessage(ctx, older.ConversationID, Inbound, "older")
	newer, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	_, _ = store.AppendMessage(ctx, newer.ConversationID, Inbound, "newer")
	// Touch `older` again so it becomes the most recently active.
	_, _ = store.AppendMessage(ctx, older.ConversationID, Outbound, "older reply")

	sink, events := capture()
	dispatchJSON(t, bareDispatcher(store), map[string]any{"action": "list_conversations", "requestId": "r1"}, sink)

	if len(*events) != 1 || (*events)[0]["type"] != "immediate_response" {
		t.Fatalf("want one immediate_response, got %+v", *events)
	}
	ev := (*events)[0]
	if ev["message"] != "Conversations" || mustInt(t, ev["status"]) != 200 {
		t.Fatalf("bad envelope: %+v", ev)
	}
	data := ev["data"].(map[string]any)
	convs := data["conversations"].([]map[string]any)
	if len(convs) != 2 {
		t.Fatalf("want 2 conversations, got %d", len(convs))
	}
	if convs[0]["conversationId"] != older.ConversationID {
		t.Errorf("most-recent-first order wrong: first = %v, want %q", convs[0]["conversationId"], older.ConversationID)
	}
	if convs[0]["title"] != "older" || convs[1]["title"] != "newer" {
		t.Errorf("titles = %v / %v, want older / newer", convs[0]["title"], convs[1]["title"])
	}
	if _, ok := convs[0]["updatedAt"].(string); !ok {
		t.Errorf("updatedAt not an ISO string: %v", convs[0]["updatedAt"])
	}
}

func TestListConversationsRespectsLimit(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	for i := 0; i < 5; i++ {
		s, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
		_, _ = store.AppendMessage(ctx, s.ConversationID, Inbound, "msg")
	}

	sink, events := capture()
	dispatchJSON(t, bareDispatcher(store), map[string]any{"action": "list_conversations", "requestId": "r1", "limit": 2}, sink)

	convs := (*events)[0]["data"].(map[string]any)["conversations"].([]map[string]any)
	if len(convs) != 2 {
		t.Fatalf("limit=2 not honored, got %d", len(convs))
	}
}

func TestResumeSessionBindsExistingConversation(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()

	// Seed a conversation with history.
	orig, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	_, _ = store.AppendMessage(ctx, orig.ConversationID, Inbound, "prior turn")

	t.Run("known conversationId resumes and preserves history", func(t *testing.T) {
		resumed, wasResumed, err := store.ResumeSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true}, orig.ConversationID)
		if err != nil {
			t.Fatalf("resume: %v", err)
		}
		if !wasResumed {
			t.Fatal("wasResumed = false, want true for a known conversation")
		}
		if resumed.ConversationID != orig.ConversationID {
			t.Errorf("bound conversationId = %q, want %q", resumed.ConversationID, orig.ConversationID)
		}
		if resumed.SessionID == orig.SessionID {
			t.Error("resume reused the session id; a fresh session should be minted")
		}
		msgs, _ := store.ListMessages(ctx, resumed.ConversationID, 0)
		if len(msgs) != 1 || msgs[0].Text != "prior turn" {
			t.Errorf("resume did not preserve history: %+v", msgs)
		}
	})

	t.Run("unknown conversationId falls back to a fresh conversation", func(t *testing.T) {
		s, wasResumed, _ := store.ResumeSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true}, "does-not-exist")
		if wasResumed {
			t.Fatal("wasResumed = true for an unknown conversation")
		}
		if s.ConversationID == "does-not-exist" || s.ConversationID == orig.ConversationID {
			t.Errorf("unknown id should mint a fresh conversation, got %q", s.ConversationID)
		}
		msgs, _ := store.ListMessages(ctx, s.ConversationID, 0)
		if len(msgs) != 0 {
			t.Errorf("fresh conversation should start empty, got %+v", msgs)
		}
	})

	t.Run("empty conversationId mints fresh (create_conversation_session default)", func(t *testing.T) {
		s, wasResumed, _ := store.ResumeSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true}, "")
		if wasResumed {
			t.Fatal("wasResumed = true for an empty conversationId")
		}
		if s.ConversationID == "" || s.ConversationID == orig.ConversationID {
			t.Errorf("empty id should mint a fresh conversation, got %q", s.ConversationID)
		}
	})
}

func TestCreateSessionResumeEchoesConversationId(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	orig, _ := store.CreateSession(ctx, "agent", "U", "u@example.com", ConversationScope{Unscoped: true})
	_, _ = store.AppendMessage(ctx, orig.ConversationID, Inbound, "hi")

	sink, events := capture()
	dispatchJSON(t, bareDispatcher(store), map[string]any{
		"action":         "create_conversation_session",
		"requestId":      "r1",
		"agentId":        "agent",
		"conversationId": orig.ConversationID,
	}, sink)

	if len(*events) != 1 || (*events)[0]["type"] != "immediate_response" {
		t.Fatalf("want immediate_response, got %+v", *events)
	}
	data := (*events)[0]["data"].(map[string]any)
	if data["conversationId"] != orig.ConversationID {
		t.Errorf("resumed session echoed conversationId %v, want %q", data["conversationId"], orig.ConversationID)
	}
}

// agentId is REQUIRED by the Request schema, so absent-or-blank is a malformed request —
// not an agentless session. Asserts BOTH halves: the request is rejected, and NOTHING is
// persisted. A rejection that still writes a row is the same bug wearing an error message.
func TestCreateSessionRejectsBlankAgentID(t *testing.T) {
	for _, tc := range []struct {
		name  string
		frame map[string]any
	}{
		{"absent", map[string]any{"action": "create_conversation_session", "requestId": "r1"}},
		{"empty", map[string]any{"action": "create_conversation_session", "requestId": "r1", "agentId": ""}},
		{"whitespace", map[string]any{"action": "create_conversation_session", "requestId": "r1", "agentId": "   "}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			store := NewInMemorySessionStore()
			d := authEnabledDispatcher(store, AnonymousAccess)

			sink, events := capture()
			dispatchJSON(t, d, tc.frame, sink)

			if len(*events) != 1 || errCode((*events)[0]) != "VALIDATION_ERROR" {
				t.Fatalf("want a VALIDATION_ERROR, got %+v", *events)
			}
			// …and no conversation was persisted.
			convs, err := store.ListConversations(context.Background(), ConversationScope{})
			if err != nil {
				t.Fatalf("ListConversations: %v", err)
			}
			if len(convs) != 0 {
				t.Errorf("a rejected create persisted %d conversation(s)", len(convs))
			}
		})
	}
}

// A reconnect IS a resume: the client re-opens the socket and re-issues
// create_conversation_session with the same conversationId, which mints a NEW session id
// on a NEW dispatcher. `supports` — the render-capability list gating the whole Rich
// Interactions framework — used to live in a per-connection map, so unless the client
// re-declared it on every reconnect the server forgot it could render cards and every
// kind silently degraded to its conversational fallback: no error, no event, nothing on
// the wire to notice. It now rides the CONVERSATION, so an omitting reconnect inherits
// it. th-13df6d.
func TestReconnectKeepsDeclaredCapabilities(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()

	// create_conversation_session on a FRESH dispatcher each time — that is what a
	// reconnect is — returning what the next turn on that session would gate on.
	reconnect := func(t *testing.T, frame map[string]any) map[string]bool {
		t.Helper()
		sink, events := capture()
		dispatchJSON(t, bareDispatcher(store), frame, sink)
		if len(*events) != 1 || (*events)[0]["type"] != "immediate_response" {
			t.Fatalf("create_conversation_session: want immediate_response, got %+v", *events)
		}
		data := (*events)[0]["data"].(map[string]any)
		if frame["conversationId"] != nil && data["conversationId"] != frame["conversationId"] {
			t.Fatalf("reconnect resumed conversation %v, want %v", data["conversationId"], frame["conversationId"])
		}
		session, err := store.GetSession(ctx, data["sessionId"].(string))
		if err != nil || session == nil {
			t.Fatalf("GetSession(%v): %v", data["sessionId"], err)
		}
		// The exact expression handleSendMessage hands the turn runner.
		return capabilitySet(session.Supports)
	}

	// First connection: declares the capability.
	first := map[string]any{
		"action": "create_conversation_session", "requestId": "r-conn-1",
		"agentId": "agent", "supports": []string{"identity_form"},
	}
	sink, events := capture()
	dispatchJSON(t, bareDispatcher(store), first, sink)
	convID := (*events)[0]["data"].(map[string]any)["conversationId"].(string)

	// Reconnect with `supports` OMITTED — exactly what a widget resuming from its
	// stored conversationId sends. This is where the feature used to go dark.
	if caps := reconnect(t, map[string]any{
		"action": "create_conversation_session", "requestId": "r-conn-2",
		"agentId": "agent", "conversationId": convID,
	}); !caps["identity_form"] {
		t.Fatalf("a reconnect omitting 'supports' lost the conversation's capabilities: %v", caps)
	}

	// A resume that DECLARES wins — including an explicit `[]`, which is how a
	// text-only channel resuming a rich conversation opts out of cards it can't render.
	if caps := reconnect(t, map[string]any{
		"action": "create_conversation_session", "requestId": "r-conn-3",
		"agentId": "agent", "conversationId": convID, "supports": []string{},
	}); len(caps) != 0 {
		t.Fatalf("an explicit empty 'supports' must declare text-only, got %v", caps)
	}

	// …and that opt-out is itself durable: the next omitting reconnect must not
	// resurrect the capability from a stale record.
	if caps := reconnect(t, map[string]any{
		"action": "create_conversation_session", "requestId": "r-conn-4",
		"agentId": "agent", "conversationId": convID,
	}); len(caps) != 0 {
		t.Fatalf("the text-only declaration did not replace the durable record, got %v", caps)
	}
}
