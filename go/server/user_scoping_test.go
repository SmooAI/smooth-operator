package server

import (
	"context"
	"reflect"
	"sort"
	"testing"
)

// Adversarial coverage for th-8fe998: conversations are scoped to the connection's
// AUTHENTICATED principal. These tests are written from the attacker's side — user A holding
// a valid token trying to see, resume, or read user B's conversations — because the bug being
// fixed (list_conversations returning every user's conversations) passed every test written
// from the honest user's side.

// authedDispatcher builds a dispatcher for a connection authenticated as email, as an
// auth-ENABLED server would resolve it.
func authedDispatcher(store SessionStore, email string) *FrameDispatcher {
	access := AccessContext{Principal: Principal{Sub: email, Org: "acme", Email: email}, AuthEnabled: true}
	return NewFrameDispatcher(store, nil, access, "", nil, nil, nil, nil, nil, "", nil, nil, nil, nil)
}

// scopeFor is the scope an auth-enabled connection for email carries.
func scopeFor(email string) ConversationScope { return ConversationScope{Email: email} }

// seedConversation creates a conversation owned by ownerEmail with one inbound message, and
// returns the session it was created through.
func seedConversation(t *testing.T, store SessionStore, ownerEmail, text string) StoredSession {
	t.Helper()
	s, err := store.CreateSession(context.Background(), "agent", "U", ownerEmail, scopeFor(ownerEmail))
	if err != nil {
		t.Fatalf("create session for %s: %v", ownerEmail, err)
	}
	if _, err := store.AppendMessage(context.Background(), s.ConversationID, Inbound, text); err != nil {
		t.Fatalf("append for %s: %v", ownerEmail, err)
	}
	return s
}

// listConversationIDs drives list_conversations as email and returns the ids it returned.
func listConversationIDs(t *testing.T, store SessionStore, email string) []string {
	t.Helper()
	sink, events := capture()
	dispatchJSON(t, authedDispatcher(store, email), map[string]any{"action": "list_conversations", "requestId": "r1"}, sink)
	if len(*events) != 1 {
		t.Fatalf("want 1 event, got %+v", *events)
	}
	convs := (*events)[0]["data"].(map[string]any)["conversations"].([]map[string]any)
	ids := make([]string, 0, len(convs))
	for _, c := range convs {
		ids = append(ids, c["conversationId"].(string))
	}
	return ids
}

func TestListConversationsScopedToPrincipal(t *testing.T) {
	store := NewInMemorySessionStore()
	a := seedConversation(t, store, "a@example.com", "alice secret")
	b := seedConversation(t, store, "b@example.com", "bob secret")

	got := listConversationIDs(t, store, "a@example.com")
	if !reflect.DeepEqual(got, []string{a.ConversationID}) {
		t.Fatalf("A's list = %v, want exactly [%s] — B's conversation must never appear", got, a.ConversationID)
	}

	got = listConversationIDs(t, store, "b@example.com")
	if !reflect.DeepEqual(got, []string{b.ConversationID}) {
		t.Fatalf("B's list = %v, want exactly [%s]", got, b.ConversationID)
	}
}

// The original bug returned every user's conversations. With many other users present, a
// scoped list must still return the caller's own — not an empty page — which is what a filter
// applied AFTER the limit would produce.
func TestListConversationsFiltersBeforeLimit(t *testing.T) {
	store := NewInMemorySessionStore()
	// 60 conversations belonging to other people, more than defaultListLimit (50).
	for i := 0; i < 60; i++ {
		seedConversation(t, store, "other@example.com", "noise")
	}
	mine := seedConversation(t, store, "a@example.com", "mine")

	got := listConversationIDs(t, store, "a@example.com")
	if !reflect.DeepEqual(got, []string{mine.ConversationID}) {
		t.Fatalf("scoped list = %v, want exactly [%s]; a filter applied after the limit "+
			"would have returned an empty page here", got, mine.ConversationID)
	}
}

// getConversationMessagesEvent drives get_conversation_messages as email and returns the raw
// single event, so tests can compare whole payloads.
func getConversationMessagesEvent(t *testing.T, store SessionStore, email, sessionID string) map[string]any {
	t.Helper()
	sink, events := capture()
	dispatchJSON(t, authedDispatcher(store, email), map[string]any{
		"action": "get_conversation_messages", "requestId": "r1", "sessionId": sessionID,
	}, sink)
	if len(*events) != 1 {
		t.Fatalf("want 1 event, got %+v", *events)
	}
	return (*events)[0]
}

func TestGetConversationMessagesDeniesOtherUsersSession(t *testing.T) {
	store := NewInMemorySessionStore()
	b := seedConversation(t, store, "b@example.com", "bob secret")

	ev := getConversationMessagesEvent(t, store, "a@example.com", b.SessionID)
	if ev["type"] != "error" || errCode(ev) != "SESSION_NOT_FOUND" {
		t.Fatalf("A reading B's session got %+v, want a SESSION_NOT_FOUND error", ev)
	}
	// The leak the fix exists to close: not one byte of B's messages may reach A.
	if data, ok := ev["data"].(map[string]any); ok {
		if _, leaked := data["messages"]; leaked {
			t.Fatalf("denied read leaked messages: %+v", ev)
		}
	}
}

// THE EXISTENCE-ORACLE TEST. If "someone else's session" and "a session id that never
// existed" produce even slightly different responses, an attacker can enumerate valid session
// ids by diffing responses — so this asserts on the FULL payloads, not just the error code.
func TestNotYoursIsIndistinguishableFromNotFound(t *testing.T) {
	store := NewInMemorySessionStore()
	b := seedConversation(t, store, "b@example.com", "bob secret")

	notYours := comparable(getConversationMessagesEvent(t, store, "a@example.com", b.SessionID))
	neverExisted := comparable(getConversationMessagesEvent(t, store, "a@example.com", "00000000-0000-0000-0000-000000000000"))

	if !reflect.DeepEqual(notYours, neverExisted) {
		t.Fatalf("responses differ — this is an existence oracle for enumerating session ids.\n"+
			"  not-yours:      %+v\n  never-existed:  %+v", notYours, neverExisted)
	}

	// Same requirement on get_session, which reads the same store primitive.
	sinkA, evA := capture()
	dispatchJSON(t, authedDispatcher(store, "a@example.com"),
		map[string]any{"action": "get_session", "requestId": "r1", "sessionId": b.SessionID}, sinkA)
	sinkB, evB := capture()
	dispatchJSON(t, authedDispatcher(store, "a@example.com"),
		map[string]any{"action": "get_session", "requestId": "r1", "sessionId": "nope"}, sinkB)
	if a, b := comparable((*evA)[0]), comparable((*evB)[0]); !reflect.DeepEqual(a, b) {
		t.Fatalf("get_session is an existence oracle.\n  not-yours: %+v\n  unknown:   %+v", a, b)
	}
	if len(*evA) != len(*evB) {
		t.Fatalf("event COUNT differs (%d vs %d) — that alone is an oracle", len(*evA), len(*evB))
	}
}

// Resuming someone else's conversation must behave exactly like resuming an id that never
// existed: a brand-new, empty conversation. Anything else (an error, the real id echoed back)
// tells the attacker their guess was real.
func TestResumeOtherUsersConversationIsIndistinguishableFromUnknown(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	b := seedConversation(t, store, "b@example.com", "bob secret")

	stolen, resumed, err := store.ResumeSession(ctx, "agent", "U", "a@example.com", scopeFor("a@example.com"), b.ConversationID)
	if err != nil {
		t.Fatalf("resume: %v", err)
	}
	if resumed {
		t.Fatal("A resumed B's conversation — cross-user resume must never bind")
	}
	if stolen.ConversationID == b.ConversationID {
		t.Fatalf("A's session bound to B's conversation %q", b.ConversationID)
	}
	if msgs, _ := store.ListMessages(ctx, stolen.ConversationID, 0); len(msgs) != 0 {
		t.Fatalf("A's fallback conversation carries history: %+v", msgs)
	}
	// B's conversation must be untouched — not re-homed onto A.
	if got := listConversationIDs(t, store, "b@example.com"); !reflect.DeepEqual(got, []string{b.ConversationID}) {
		t.Fatalf("B's conversation was disturbed by A's resume attempt: %v", got)
	}

	// And over the wire, the two cases are byte-identical apart from the freshly minted ids.
	steal := createSessionData(t, store, "a@example.com", b.ConversationID)
	unknown := createSessionData(t, store, "a@example.com", "does-not-exist")
	if steal["conversationId"] == b.ConversationID {
		t.Fatalf("create_conversation_session echoed B's conversation id back to A")
	}
	if !reflect.DeepEqual(keysOf(steal), keysOf(unknown)) {
		t.Fatalf("response shapes differ: %v vs %v", keysOf(steal), keysOf(unknown))
	}
}

// createSessionData drives create_conversation_session as email and returns its data payload.
func createSessionData(t *testing.T, store SessionStore, email, conversationID string) map[string]any {
	t.Helper()
	sink, events := capture()
	dispatchJSON(t, authedDispatcher(store, email), map[string]any{
		"action": "create_conversation_session", "requestId": "r1",
		"agentId": "agent", "conversationId": conversationID,
	}, sink)
	if len(*events) != 1 || (*events)[0]["type"] != "immediate_response" {
		t.Fatalf("want immediate_response, got %+v", *events)
	}
	return (*events)[0]["data"].(map[string]any)
}

// errCode pulls the error code out of an error event's descriptor.
func errCode(ev map[string]any) string {
	d, ok := ev["error"].(map[string]any)
	if !ok {
		return ""
	}
	code, _ := d["code"].(string)
	return code
}

// comparable strips the wall-clock timestamp every event carries, so two responses can be
// compared for the thing that actually matters: whether they are distinguishable in CONTENT.
// The timestamp varies between any two calls and is not an oracle.
func comparable(ev map[string]any) map[string]any {
	out := make(map[string]any, len(ev))
	for k, v := range ev {
		if k == "timestamp" {
			continue
		}
		out[k] = v
	}
	return out
}

func keysOf(m map[string]any) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

// THE SPOOFING TEST. userEmail is a client-supplied frame field. A caller who types someone
// else's address into it must NOT inherit that person's scope — the connection's principal
// decides ownership, and nothing the client sends can override it.
func TestClientSuppliedEmailCannotStealScope(t *testing.T) {
	store := NewInMemorySessionStore()
	victim := seedConversation(t, store, "victim@example.com", "victim secret")

	// Attacker authenticates as themselves but claims the victim's email in the frame.
	sink, events := capture()
	dispatchJSON(t, authedDispatcher(store, "attacker@example.com"), map[string]any{
		"action": "create_conversation_session", "requestId": "r1",
		"agentId": "agent", "userName": "Victim", "userEmail": "victim@example.com",
	}, sink)
	if len(*events) != 1 {
		t.Fatalf("want 1 event, got %+v", *events)
	}

	// The spoofed email must not have widened what the attacker can list.
	got := listConversationIDs(t, store, "attacker@example.com")
	for _, id := range got {
		if id == victim.ConversationID {
			t.Fatalf("client-supplied userEmail granted the victim's scope: %v", got)
		}
	}

	// Nor may it have stamped the victim as owner of the attacker's new session.
	sid := (*events)[0]["data"].(map[string]any)["sessionId"].(string)
	s, _ := store.GetSession(context.Background(), sid)
	if s.OwnerEmail != "attacker@example.com" {
		t.Fatalf("OwnerEmail = %q, want the authenticated principal %q — ownership must come "+
			"from the token, not the frame", s.OwnerEmail, "attacker@example.com")
	}
	// The client-supplied address survives only as the OTP delivery contact.
	if s.ContactEmail != "victim@example.com" {
		t.Fatalf("ContactEmail = %q, want the frame value preserved for OTP delivery", s.ContactEmail)
	}
}

// Auth enabled but the token carries no email: no OWNED conversation is visible. Such a
// principal owns nothing and can therefore reach nothing that anyone else owns — the leak a
// well-meaning "if email == \"\" { return everything }" fallback would open.
//
// Owner-LESS conversations are a separate population and DO stay reachable (th-909995): this
// principal's own sessions are stamped with no owner, so denying them means denying it the
// product entirely. TestEmaillessPrincipalCanConverseOnAuthEnabledServer covers that side.
func TestAuthEnabledWithoutEmailSeesNoOwnedConversations(t *testing.T) {
	store := NewInMemorySessionStore()
	a := seedConversation(t, store, "a@example.com", "alice secret")
	unowned, _ := store.CreateSession(context.Background(), "agent", "U", "", ConversationScope{Unscoped: true})
	_, _ = store.AppendMessage(context.Background(), unowned.ConversationID, Inbound, "auth-disabled era")

	emailless := AccessContext{Principal: Principal{Sub: "svc", Org: "acme"}, AuthEnabled: true}
	d := NewFrameDispatcher(store, nil, emailless, "", nil, nil, nil, nil, nil, "", nil, nil, nil, nil)

	sink, events := capture()
	dispatchJSON(t, d, map[string]any{"action": "list_conversations", "requestId": "r1"}, sink)
	convs := (*events)[0]["data"].(map[string]any)["conversations"].([]map[string]any)
	for _, c := range convs {
		if c["conversationId"] == a.ConversationID {
			t.Fatalf("emailless principal saw A's OWNED conversation: %+v", convs)
		}
	}

	// A's session itself stays hidden; the owner-less one is reachable.
	for _, tc := range []struct {
		name    string
		id      string
		wantErr bool
	}{
		{"owned by A", a.SessionID, true},
		{"owner-less", unowned.SessionID, false},
	} {
		sink, events := capture()
		dispatchJSON(t, d, map[string]any{
			"action": "get_conversation_messages", "requestId": "r1", "sessionId": tc.id,
		}, sink)
		denied := errCode((*events)[0]) == "SESSION_NOT_FOUND"
		if denied != tc.wantErr {
			t.Fatalf("emailless principal on session %s: denied=%v, want denied=%v (%+v)",
				tc.name, denied, tc.wantErr, (*events)[0])
		}
	}
}

// Auth disabled (no verifier configured) is the ONE unscoped path and must keep working
// exactly as before — this is the local/dev single-tenant case.
func TestAuthDisabledStaysUnscoped(t *testing.T) {
	ctx := context.Background()
	store := NewInMemorySessionStore()
	unscoped := ConversationScope{Unscoped: true}
	one, _ := store.CreateSession(ctx, "agent", "U", "u1@example.com", unscoped)
	_, _ = store.AppendMessage(ctx, one.ConversationID, Inbound, "first")
	two, _ := store.CreateSession(ctx, "agent", "U", "u2@example.com", unscoped)
	_, _ = store.AppendMessage(ctx, two.ConversationID, Inbound, "second")

	// bareDispatcher carries the zero AccessContext — AuthEnabled false — the no-auth server.
	sink, events := capture()
	dispatchJSON(t, bareDispatcher(store), map[string]any{"action": "list_conversations", "requestId": "r1"}, sink)
	convs := (*events)[0]["data"].(map[string]any)["conversations"].([]map[string]any)
	if len(convs) != 2 {
		t.Fatalf("auth-disabled list returned %d conversations, want both (2)", len(convs))
	}

	// Resume across sessions still binds, and reads still succeed.
	if _, resumed, _ := store.ResumeSession(ctx, "agent", "U", "u2@example.com", unscoped, one.ConversationID); !resumed {
		t.Fatal("auth-disabled resume stopped binding an existing conversation")
	}
	ev := map[string]any{"action": "get_conversation_messages", "requestId": "r1", "sessionId": two.SessionID}
	sink2, events2 := capture()
	dispatchJSON(t, bareDispatcher(store), ev, sink2)
	if (*events2)[0]["type"] != "immediate_response" {
		t.Fatalf("auth-disabled read denied: %+v", (*events2)[0])
	}
}

func TestConversationScopeAllows(t *testing.T) {
	tests := []struct {
		name  string
		scope ConversationScope
		owner string
		want  bool
	}{
		{"unscoped sees owned", ConversationScope{Unscoped: true}, "a@example.com", true},
		{"unscoped sees owner-less", ConversationScope{Unscoped: true}, "", true},
		{"match", scopeFor("a@example.com"), "a@example.com", true},
		{"match ignores email casing", scopeFor("Alice@Example.COM"), "alice@example.com", true},
		{"mismatch", scopeFor("a@example.com"), "b@example.com", false},
		// Owner-less conversations have nobody to enforce on behalf of, so they stay
		// reachable — otherwise anonymous and emailless principals are locked out of the
		// sessions they themselves created. th-909995 (Option B).
		{"scoped sees owner-less", scopeFor("a@example.com"), "", true},
		{"zero value denies OWNED conversations", ConversationScope{}, "a@example.com", false},
		{"zero value sees owner-less", ConversationScope{}, "", true},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			if got := tc.scope.Allows(tc.owner); got != tc.want {
				t.Fatalf("Allows(%q) = %v, want %v", tc.owner, got, tc.want)
			}
		})
	}
}

// The verifier must distinguish "no auth configured" from "auth configured, token rejected".
// Collapsing the two hands a forged token the unscoped view of everything.
func TestVerifierScopeDerivation(t *testing.T) {
	if s := (PermissiveVerifier{}).Resolve("").ConversationScope(); !s.Unscoped {
		t.Fatal("no-auth verifier must yield the unscoped scope")
	}

	v := NewLocalTokenVerifier("shhh")
	for _, tok := range []string{"", "garbage", "a.b.c", "not.a.jwt.at.all"} {
		access := v.Resolve(tok)
		if !access.AuthEnabled {
			t.Fatalf("rejected token %q lost AuthEnabled — it would be treated as no-auth", tok)
		}
		s := access.ConversationScope()
		if s.Unscoped || s.Email != "" {
			t.Fatalf("rejected token %q yielded scope %+v, want fail-closed zero value", tok, s)
		}
		if s.Allows("a@example.com") {
			t.Fatalf("rejected token %q can still see another user's OWNED conversation", tok)
		}
	}
}

func TestClaimsToAccessCarriesEmail(t *testing.T) {
	access, err := claimsToAccess([]byte(`{"sub":"u1","org":"acme","email":" a@example.com "}`))
	if err != nil {
		t.Fatalf("claims: %v", err)
	}
	if access.Principal.Email != "a@example.com" {
		t.Fatalf("Email = %q, want the trimmed claim", access.Principal.Email)
	}
	if !access.AuthEnabled {
		t.Fatal("a verified token must be AuthEnabled")
	}
	if got := access.ConversationScope(); got.Unscoped || got.Email != "a@example.com" {
		t.Fatalf("scope = %+v, want scoped to a@example.com", got)
	}

	// A token with no email claim must NOT be defaulted into some placeholder identity.
	noEmail, err := claimsToAccess([]byte(`{"sub":"u1","org":"acme"}`))
	if err != nil {
		t.Fatalf("claims: %v", err)
	}
	if noEmail.Principal.Email != "" {
		t.Fatalf("Email = %q, want empty (no default identity)", noEmail.Principal.Email)
	}
	if s := noEmail.ConversationScope(); s.Unscoped {
		t.Fatal("emailless token widened to unscoped — this is the fail-open the fix forbids")
	}
}
