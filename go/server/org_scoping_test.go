package server

import "testing"

// Org is the OUTER scope, applied before ownership.
//
// The gap this closes: Allows() returned true for ANY ownerless conversation
// (deliberate — th-909995 Option B keeps anonymous/emailless/legacy sessions
// reachable), and org was not consulted at all. So an ownerless conversation
// belonging to another org was readable by anyone holding its id — authorization
// resting on an unguessable UUID, which leaks through logs, referrers and
// screenshots.

func TestOrgIsCheckedBeforeOwnership(t *testing.T) {
	orgA := ConversationScope{Email: "a@example.com", OrgID: "org-a"}

	// THE GAP: an OWNERLESS conversation in another org used to be readable by
	// anyone, because the ownerless branch returned true before org was consulted.
	if orgA.Allows("", "org-b") {
		t.Error("an ownerless conversation in another org must NOT be visible")
	}
	// Same conversation in the caller's own org stays reachable — th-909995 intact.
	if !orgA.Allows("", "org-a") {
		t.Error("an ownerless conversation in the caller's own org must stay visible")
	}
	// An OWNED conversation in another org is denied even when the emails match,
	// so a shared email across orgs cannot cross the boundary.
	if orgA.Allows("a@example.com", "org-b") {
		t.Error("org must be checked before ownership, even for a matching email")
	}
	if !orgA.Allows("a@example.com", "org-a") {
		t.Error("owner + org both matching must be visible")
	}
}

func TestUnscopedStillSeesEverything(t *testing.T) {
	// Auth-disabled dev is the one unscoped path and must be untouched.
	unscoped := ConversationScope{Unscoped: true}
	for _, org := range []string{"", "org-a", "org-b"} {
		if !unscoped.Allows("someone@example.com", org) {
			t.Errorf("unscoped must see org %q", org)
		}
	}
}

func TestUnrecordedOrgFallsThroughToOwnership(t *testing.T) {
	// Rows created before org capture carry no org. Denying them would lock people
	// out of conversations they already own, so they fall through to the ownership
	// check rather than being denied outright.
	orgA := ConversationScope{Email: "a@example.com", OrgID: "org-a"}
	if !orgA.Allows("", "") {
		t.Error("an unrecorded-org ownerless conversation must stay reachable")
	}
	if !orgA.Allows("a@example.com", "") {
		t.Error("an unrecorded-org conversation the caller owns must stay reachable")
	}
	if orgA.Allows("b@example.com", "") {
		t.Error("ownership must still be enforced when the org is unrecorded")
	}
}

func TestCreatedConversationsRecordTheOrg(t *testing.T) {
	// End to end on the ACTUAL gap: an OWNERLESS conversation (an emailless
	// principal — the population th-909995 keeps reachable). Ownership cannot block
	// this one, so only the org check can. Use an owned conversation here and the
	// test passes with the org check removed, proving nothing.
	store := NewInMemorySessionStore()
	ctx := t.Context()

	a := ConversationScope{OrgID: "org-a"} // no Email → ownerless conversation
	session, err := store.CreateSession(ctx, "agent", "", "", a)
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	if session.OwnerOrg != "org-a" || session.OwnerEmail != "" {
		t.Fatalf("session = %+v, want ownerless in org-a", session)
	}

	// Its own org can still resume it — th-909995 intact.
	if _, resumed, err := store.ResumeSession(ctx, "agent", "", "", a, session.ConversationID); err != nil || !resumed {
		t.Errorf("same org must resume its own ownerless conversation: resumed=%v err=%v", resumed, err)
	}

	// Another org must NOT, and must not be able to tell it from an unknown id.
	b := ConversationScope{OrgID: "org-b"}
	other, resumed, err := store.ResumeSession(ctx, "agent", "", "", b, session.ConversationID)
	if err != nil {
		t.Fatalf("cross-org resume: %v", err)
	}
	if resumed {
		t.Error("another org must never resume this ownerless conversation")
	}
	if other.ConversationID == session.ConversationID {
		t.Error("another org must not be bound to this conversation id")
	}
}
