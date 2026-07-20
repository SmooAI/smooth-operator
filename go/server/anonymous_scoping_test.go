package server

import (
	"context"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// Coverage for th-909995: the ownership rule must close the P0 (authenticated A reaching
// into authenticated B's OWNED session) WITHOUT locking out the principals that have no
// email to own with.
//
// The rule shipped in #298 — "deny unless scope.Email != \"\" && owner == scope.Email" —
// denied everything to any connection whose principal carried no email claim. On an
// auth-enabled server that is an outage, not a hardening: AnonymousPrincipal has no email,
// so an anonymous visitor could not read or even send into the session it had just created.
// The .NET sibling caught this only because it happened to have a WebSocket ACL test that
// authenticates without an email claim; Go had no such test, which is why it shipped here.
// These are that missing test.

// authEnabledDispatcher builds a dispatcher for a connection resolved by an auth-ENABLED
// server as the given access context, with a mock LLM so send_message can run a real turn.
func authEnabledDispatcher(store SessionStore, access AccessContext) *FrameDispatcher {
	llm := core.NewMockLlmProvider().PushText("ack")
	return NewFrameDispatcher(store, llm, access, "", nil, nil, nil, nil, nil, "", nil, nil, nil, nil)
}

// emaillessAuthedAccess is a VERIFIED principal that simply carries no email claim — the
// exact token shape (sub/org/role/groups, no email) that hung the .NET ACL test.
var emaillessAuthedAccess = AccessContext{
	Principal:   Principal{Sub: "u1", Org: "acme", Role: "basic", Groups: []string{"staff"}},
	AuthEnabled: true,
}

// sendMessage drives a full send_message turn and returns the events it produced.
func sendMessage(t *testing.T, d *FrameDispatcher, sessionID, text string) []map[string]any {
	t.Helper()
	sink, events := capture()
	dispatchJSON(t, d, map[string]any{
		"action": "send_message", "requestId": "r-send", "sessionId": sessionID, "message": text,
	}, sink)
	d.WaitForTurns()
	return *events
}

// deniedAsNotFound reports whether the event set is the scoping denial (and nothing else).
func deniedAsNotFound(events []map[string]any) bool {
	return len(events) == 1 && errCode(events[0]) == "SESSION_NOT_FOUND"
}

// conversationTexts returns every message text stored on a conversation, so a test can
// assert that a denied write left the victim's log untouched.
func conversationTexts(t *testing.T, store SessionStore, conversationID string) []string {
	t.Helper()
	msgs, err := store.ListMessages(context.Background(), conversationID, 0)
	if err != nil {
		t.Fatalf("list messages: %v", err)
	}
	out := make([]string, 0, len(msgs))
	for _, m := range msgs {
		out = append(out, m.Text)
	}
	return out
}

// THE MISSING TEST. A principal with no email, on an AUTH-ENABLED server, must be able to
// create a session, read it back, list it, and send into it. Under the #298 rule every one
// of these four steps failed — the caller was locked out of the session it had just made.
//
// Both rows are real production identities: AnonymousPrincipal is what every unauthenticated
// visitor to an auth-enabled server gets (public-agent chat, a supported Smoo AI scenario),
// and the emailless row is a verified token whose IdP simply issues no email claim.
func TestEmaillessPrincipalCanConverseOnAuthEnabledServer(t *testing.T) {
	for _, tc := range []struct {
		name   string
		access AccessContext
	}{
		{"anonymous connection", anonymousAuthedAccess},
		{"authenticated principal with no email claim", emaillessAuthedAccess},
	} {
		t.Run(tc.name, func(t *testing.T) {
			store := NewInMemorySessionStore()
			d := authEnabledDispatcher(store, tc.access)

			// 1. create — must succeed, and must stamp NO owner (there is no email to own with).
			sink, events := capture()
			dispatchJSON(t, d, map[string]any{
				"action": "create_conversation_session", "requestId": "r1", "agentId": "agent",
			}, sink)
			if len(*events) != 1 || (*events)[0]["type"] != "immediate_response" {
				t.Fatalf("create denied for %s: %+v", tc.name, *events)
			}
			sessionID := (*events)[0]["data"].(map[string]any)["sessionId"].(string)
			session, _ := store.GetSession(context.Background(), sessionID)
			if session.OwnerEmail != "" {
				t.Fatalf("OwnerEmail = %q, want empty — this principal has no email to own with", session.OwnerEmail)
			}

			// 2. read its OWN session back — the step #298 broke first.
			sink, events = capture()
			dispatchJSON(t, d, map[string]any{
				"action": "get_conversation_messages", "requestId": "r1", "sessionId": sessionID,
			}, sink)
			if errCode((*events)[0]) == "SESSION_NOT_FOUND" {
				t.Fatalf("%s was denied its OWN session — it cannot use the product: %+v", tc.name, (*events)[0])
			}

			// 3. send into it — the write path #308 broke in .NET, and the reason #309 reverted.
			sent := sendMessage(t, d, sessionID, "hello from an emailless caller")
			if deniedAsNotFound(sent) {
				t.Fatalf("%s could not send into its own session: %+v", tc.name, sent)
			}
			if texts := conversationTexts(t, store, session.ConversationID); len(texts) == 0 {
				t.Fatalf("%s's message never reached the conversation log", tc.name)
			}

			// 4. and the conversation shows up in its own list.
			sink, events = capture()
			dispatchJSON(t, d, map[string]any{"action": "list_conversations", "requestId": "r1"}, sink)
			convs := (*events)[0]["data"].(map[string]any)["conversations"].([]map[string]any)
			found := false
			for _, c := range convs {
				if c["conversationId"] == session.ConversationID {
					found = true
				}
			}
			if !found {
				t.Fatalf("%s's own conversation missing from its list: %+v", tc.name, convs)
			}
		})
	}
}

// THE P0, STILL CLOSED. Option B relaxes ownerless sessions only. A session that HAS an
// owner stays owner-checked on BOTH the read and the write path: authenticated A must not be
// able to read B's session, and — the part that matters most — must not be able to append to
// B's conversation log.
func TestOwnedSessionStillDeniedToOtherAuthenticatedPrincipal(t *testing.T) {
	store := NewInMemorySessionStore()
	b := seedConversation(t, store, "b@example.com", "bob secret")

	attacker := authEnabledDispatcher(store, AccessContext{
		Principal:   Principal{Sub: "a", Org: "acme", Email: "a@example.com"},
		AuthEnabled: true,
	})

	// Read is refused, indistinguishably from a session id that never existed.
	sink, events := capture()
	dispatchJSON(t, attacker, map[string]any{
		"action": "get_conversation_messages", "requestId": "r1", "sessionId": b.SessionID,
	}, sink)
	if errCode((*events)[0]) != "SESSION_NOT_FOUND" {
		t.Fatalf("A read B's OWNED session — the P0 is open again: %+v", (*events)[0])
	}

	// Write is refused, and B's log is untouched.
	before := conversationTexts(t, store, b.ConversationID)
	sent := sendMessage(t, attacker, b.SessionID, "injected by the attacker")
	if !deniedAsNotFound(sent) {
		t.Fatalf("A sent into B's OWNED session — the P0 is open again: %+v", sent)
	}
	after := conversationTexts(t, store, b.ConversationID)
	if len(after) != len(before) {
		t.Fatalf("denied write still mutated B's conversation log: %v → %v", before, after)
	}
	for _, text := range after {
		if text == "injected by the attacker" {
			t.Fatalf("attacker's text landed in B's log: %v", after)
		}
	}
}

// OIDC providers disagree on email casing, and the .NET and Python siblings fold case. Go
// used a bare == , which would lock a user out of their own conversation the day their IdP
// changed the casing of the email claim.
func TestOwnerEmailComparisonIsCaseInsensitive(t *testing.T) {
	store := NewInMemorySessionStore()
	owned := seedConversation(t, store, "alice@example.com", "alice secret")

	sameUserDifferentCasing := ConversationScope{Email: "Alice@Example.COM"}
	if !sameUserDifferentCasing.Allows("alice@example.com") {
		t.Fatal("Alice@Example.COM was denied alice@example.com's own conversation")
	}
	if !(ConversationScope{Email: "alice@example.com"}).Allows("ALICE@EXAMPLE.COM") {
		t.Fatal("case folding must work in both directions")
	}
	// Folding case must not fold anything else — a different address is still denied.
	if sameUserDifferentCasing.Allows("bob@example.com") {
		t.Fatal("case-insensitive compare matched a DIFFERENT address")
	}

	// End to end through the chokepoint.
	d := authEnabledDispatcher(store, AccessContext{
		Principal:   Principal{Sub: "alice", Org: "acme", Email: "ALICE@example.com"},
		AuthEnabled: true,
	})
	sink, events := capture()
	dispatchJSON(t, d, map[string]any{
		"action": "get_conversation_messages", "requestId": "r1", "sessionId": owned.SessionID,
	}, sink)
	if errCode((*events)[0]) == "SESSION_NOT_FOUND" {
		t.Fatalf("differently-cased email denied its own session: %+v", (*events)[0])
	}
}

// An OWNED conversation must not become visible to an emailless principal just because
// ownerless ones are. This is the line Option B draws, and the thing that would make Option B
// worthless if it slipped.
func TestEmaillessPrincipalStillCannotSeeOwnedConversations(t *testing.T) {
	store := NewInMemorySessionStore()
	owned := seedConversation(t, store, "a@example.com", "alice secret")

	for name, access := range map[string]AccessContext{
		"anonymous": anonymousAuthedAccess,
		"emailless": emaillessAuthedAccess,
	} {
		d := authEnabledDispatcher(store, access)
		sink, events := capture()
		dispatchJSON(t, d, map[string]any{
			"action": "get_conversation_messages", "requestId": "r1", "sessionId": owned.SessionID,
		}, sink)
		if errCode((*events)[0]) != "SESSION_NOT_FOUND" {
			t.Fatalf("%s principal read an OWNED conversation: %+v", name, (*events)[0])
		}
		sent := sendMessage(t, d, owned.SessionID, "injected")
		if !deniedAsNotFound(sent) {
			t.Fatalf("%s principal wrote into an OWNED conversation: %+v", name, sent)
		}
	}
}
