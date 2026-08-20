package server

// Round-trip tests for the durable Postgres store, against a REAL Postgres in a
// throwaway container (testcontainers) — the Go sibling of the Rust adapter's
// conformance.rs / admin_conformance.rs and the C# PostgresSessionStoreTests.
//
// Docker is not required to build or to run the rest of the suite: if a container
// cannot start, every Postgres test SKIPS with a notice, exactly like the Rust
// conformance tests. TestMemoryStaysDefault needs no container at all — it is the
// guard that the in-memory path is untouched when SMOOTH_AGENT_STORAGE is unset.
//
// Local gotcha: on OrbStack, testcontainers' Ryuk reaper can hang before the
// database container is ever started, and these all skip on the timeout with Docker
// plainly running. `TESTCONTAINERS_RYUK_DISABLED=true go test ./...` gets past it
// (at the cost of leaving containers behind to clean up by hand). CI's plain dockerd
// runs Ryuk fine, so this stays off by default.

import (
	"context"
	"fmt"
	"os/exec"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/google/uuid"
	"github.com/testcontainers/testcontainers-go/modules/postgres"
)

// How long to wait for the Docker daemon to answer, and for a container to be
// serving. The ping is short because its only job is to turn "no daemon" into a
// fast skip; the start is generous because a cold machine pulls the image first.
const (
	dockerPingTimeout  = 10 * time.Second
	containerUpTimeout = 4 * time.Minute
)

// sharedPostgres starts ONE container for the whole package (containers are slow;
// every test namespaces its own org/conversation ids, so they don't collide) and
// caches the DSN — or the failure, so a Docker-less machine pays for it once.
var sharedPostgres = sync.OnceValues(func() (string, error) {
	// Ping first, and bound BOTH steps with a wall clock. Passing a context is not
	// enough on its own: testcontainers resolves the Docker endpoint by shelling out
	// to the docker CLI, and against a dead daemon that subprocess can block forever —
	// which turns the intended skip into a ten-minute hang that ends as a FAILURE.
	// A guard that fails open like that is worse than no guard, so the bound lives
	// outside the call rather than inside it.
	//
	// `docker version` is the probe rather than an API client because it asks the
	// SERVER (not just the client binary) and needs no extra dependency; CommandContext
	// kills it on the deadline. It does assume the docker CLI is on PATH, which every
	// environment that can run testcontainers has anyway.
	if err := within(dockerPingTimeout, func(ctx context.Context) error {
		return exec.CommandContext(ctx, "docker", "version", "--format", "{{.Server.Version}}").Run()
	}); err != nil {
		return "", fmt.Errorf("docker daemon not reachable: %w", err)
	}

	var dsn string
	err := within(containerUpTimeout, func(ctx context.Context) error {
		container, err := postgres.Run(ctx, "postgres:16-alpine", postgres.BasicWaitStrategies())
		if err != nil {
			return err
		}
		dsn, err = container.ConnectionString(ctx, "sslmode=disable")
		return err
	})
	return dsn, err
})

// within runs work with a deadline that holds even if work ignores its context. On a
// timeout the goroutine is abandoned — acceptable in a test binary that is about to
// exit, and the alternative (blocking on an unresponsive daemon) is the bug.
func within(limit time.Duration, work func(context.Context) error) error {
	ctx, cancel := context.WithTimeout(context.Background(), limit)
	defer cancel()
	done := make(chan error, 1)
	go func() { done <- work(ctx) }()
	select {
	case err := <-done:
		return err
	case <-ctx.Done():
		return fmt.Errorf("timed out after %s", limit)
	}
}

// newPostgresStore returns a store on the shared container, skipping the test when
// Docker is unavailable.
func newPostgresStore(t *testing.T) *PostgresStore {
	t.Helper()
	dsn, err := sharedPostgres()
	if err != nil {
		t.Skipf("SKIP: could not start postgres container (Docker unavailable?): %v", err)
	}
	store, err := NewPostgresStore(t.Context(), dsn)
	if err != nil {
		t.Fatalf("NewPostgresStore: %v", err)
	}
	t.Cleanup(store.Close)
	return store
}

// scopeFor builds an authenticated scope in a test-unique org, so tests can't see
// each other's rows through the shared container.
func pgScope(t *testing.T, email string) ConversationScope {
	t.Helper()
	return ConversationScope{Email: email, OrgID: "org-" + t.Name() + "-" + uuid.NewString()}
}

// ── sessions / conversations / messages ─────────────────────────────────────

// The durability claim itself: a session and its messages written through one store
// are readable through a SECOND store on the same database — i.e. they survive the
// process that wrote them, which is the whole point of this backend.
func TestPostgresStoreSurvivesANewConnection(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()
	scope := pgScope(t, "alice@example.test")

	created, err := store.CreateSession(ctx, "", "Alice", "alice@example.test", scope)
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	if _, err := uuid.Parse(created.SessionID); err != nil {
		t.Fatalf("session id is not a uuid: %q", created.SessionID)
	}
	if _, err := store.AppendMessage(ctx, created.ConversationID, Inbound, "hello"); err != nil {
		t.Fatalf("AppendMessage: %v", err)
	}
	if _, err := store.AppendMessage(ctx, created.ConversationID, Outbound, "hi there"); err != nil {
		t.Fatalf("AppendMessage: %v", err)
	}

	// A brand-new store handle — nothing carried over in process memory.
	reopened := newPostgresStore(t)

	fetched, err := reopened.GetSession(ctx, created.SessionID)
	if err != nil {
		t.Fatalf("GetSession: %v", err)
	}
	if fetched == nil {
		t.Fatal("session did not survive: GetSession returned nil")
	}
	if fetched.ConversationID != created.ConversationID || fetched.AgentID != created.AgentID ||
		fetched.AgentParticipantID != created.AgentParticipantID {
		t.Fatalf("session round-trip mismatch:\n got %+v\nwant %+v", *fetched, created)
	}
	if fetched.OwnerEmail != "alice@example.test" {
		t.Fatalf("OwnerEmail = %q, want alice@example.test", fetched.OwnerEmail)
	}
	if fetched.ContactEmail != "alice@example.test" {
		t.Fatalf("ContactEmail = %q, want alice@example.test", fetched.ContactEmail)
	}

	messages, err := reopened.ListMessages(ctx, created.ConversationID, 50)
	if err != nil {
		t.Fatalf("ListMessages: %v", err)
	}
	if len(messages) != 2 {
		t.Fatalf("got %d messages, want 2", len(messages))
	}
	if messages[0].Text != "hello" || messages[0].Direction != Inbound {
		t.Fatalf("first message = %+v, want inbound \"hello\"", messages[0])
	}
	if messages[1].Text != "hi there" || messages[1].Direction != Outbound {
		t.Fatalf("second message = %+v, want outbound \"hi there\"", messages[1])
	}
	if messages[0].CreatedAt.IsZero() {
		t.Fatal("message CreatedAt was not persisted")
	}

	if unknown, err := reopened.GetSession(ctx, "does-not-exist"); err != nil || unknown != nil {
		t.Fatalf("GetSession(unknown) = (%v, %v), want (nil, nil)", unknown, err)
	}
}

// ListMessages returns the most recent `limit`, oldest first — the in-memory contract.
func TestPostgresStoreListMessagesRespectsLimit(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()

	session, err := store.CreateSession(ctx, "", "Alice", "", pgScope(t, ""))
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	for i := range 5 {
		if _, err := store.AppendMessage(ctx, session.ConversationID, Inbound, fmt.Sprintf("m%d", i)); err != nil {
			t.Fatalf("AppendMessage: %v", err)
		}
	}

	messages, err := store.ListMessages(ctx, session.ConversationID, 2)
	if err != nil {
		t.Fatalf("ListMessages: %v", err)
	}
	if len(messages) != 2 || messages[0].Text != "m3" || messages[1].Text != "m4" {
		t.Fatalf("got %v, want the last two oldest-first (m3, m4)", texts(messages))
	}

	// A non-positive limit means "all", like the in-memory store.
	all, err := store.ListMessages(ctx, session.ConversationID, 0)
	if err != nil {
		t.Fatalf("ListMessages(0): %v", err)
	}
	if len(all) != 5 {
		t.Fatalf("ListMessages(0) returned %d messages, want all 5", len(all))
	}
}

// Resume binds to the caller's OWN conversation; someone else's takes the identical
// branch as an unknown id (fresh conversation, resumed=false) so it cannot be used to
// probe which conversation ids exist. th-8fe998.
func TestPostgresStoreResumeIsOwnerScopedWithoutAnOracle(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()

	org := "org-" + uuid.NewString()
	alice := ConversationScope{Email: "alice@example.test", OrgID: org}
	bob := ConversationScope{Email: "bob@example.test", OrgID: org}

	owned, err := store.CreateSession(ctx, "", "Alice", "alice@example.test", alice)
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}

	// Alice resumes her own — bound, and the owner is preserved.
	resumedSession, resumed, err := store.ResumeSession(ctx, "", "Alice", "alice@example.test", alice, owned.ConversationID)
	if err != nil {
		t.Fatalf("ResumeSession: %v", err)
	}
	if !resumed || resumedSession.ConversationID != owned.ConversationID {
		t.Fatalf("owner resume: resumed=%v conv=%q, want true and %q", resumed, resumedSession.ConversationID, owned.ConversationID)
	}
	if resumedSession.OwnerEmail != "alice@example.test" {
		t.Fatalf("resumed OwnerEmail = %q, want the ORIGINAL owner", resumedSession.OwnerEmail)
	}

	// Bob names Alice's conversation …
	bobSession, bobResumed, err := store.ResumeSession(ctx, "", "Bob", "bob@example.test", bob, owned.ConversationID)
	if err != nil {
		t.Fatalf("ResumeSession(other user): %v", err)
	}
	if bobResumed || bobSession.ConversationID == owned.ConversationID {
		t.Fatalf("BREACH: bob resumed alice's conversation (resumed=%v conv=%q)", bobResumed, bobSession.ConversationID)
	}
	// … and gets exactly what he gets for an id that never existed.
	unknownSession, unknownResumed, err := store.ResumeSession(ctx, "", "Bob", "bob@example.test", bob, uuid.NewString())
	if err != nil {
		t.Fatalf("ResumeSession(unknown): %v", err)
	}
	if unknownResumed || unknownSession.ConversationID == owned.ConversationID {
		t.Fatalf("unknown-id resume differed from the not-yours resume: %+v", unknownSession)
	}

	// A resume must not have re-homed the conversation onto Bob.
	after, err := store.GetSession(ctx, owned.SessionID)
	if err != nil || after == nil {
		t.Fatalf("GetSession: %v (session %v)", err, after)
	}
	if after.OwnerEmail != "alice@example.test" {
		t.Fatalf("ownership was rewritten to %q", after.OwnerEmail)
	}
}

// An ownerless conversation (auth disabled, or an emailless principal) stays reachable
// — denying it would lock anonymous visitors out of the session they just created.
// th-909995.
func TestPostgresStoreOwnerlessConversationStaysReachable(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()

	org := "org-" + uuid.NewString()
	anonymous := ConversationScope{Unscoped: true, OrgID: org}

	created, err := store.CreateSession(ctx, "", "", "", anonymous)
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	if created.OwnerEmail != "" {
		t.Fatalf("OwnerEmail = %q, want ownerless", created.OwnerEmail)
	}

	// Reachable by an authenticated principal in the same org, exactly as ConversationScope.Allows says.
	_, resumed, err := store.ResumeSession(ctx, "", "Carol", "carol@example.test",
		ConversationScope{Email: "carol@example.test", OrgID: org}, created.ConversationID)
	if err != nil {
		t.Fatalf("ResumeSession: %v", err)
	}
	if !resumed {
		t.Fatal("an ownerless conversation must stay resumable")
	}
}

// ListConversations filters in the SELECT: a user sees their own conversations and
// ownerless ones, never another user's, and never an empty conversation.
func TestPostgresStoreListConversationsIsScoped(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()

	org := "org-" + uuid.NewString()
	alice := ConversationScope{Email: "alice@example.test", OrgID: org}
	bob := ConversationScope{Email: "bob@example.test", OrgID: org}

	aliceConv, err := store.CreateSession(ctx, "", "Alice", "alice@example.test", alice)
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	if _, err := store.AppendMessage(ctx, aliceConv.ConversationID, Inbound, "alice asks"); err != nil {
		t.Fatalf("AppendMessage: %v", err)
	}
	if _, err := store.AppendMessage(ctx, aliceConv.ConversationID, Outbound, "agent answers"); err != nil {
		t.Fatalf("AppendMessage: %v", err)
	}

	bobConv, err := store.CreateSession(ctx, "", "Bob", "bob@example.test", bob)
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	if _, err := store.AppendMessage(ctx, bobConv.ConversationID, Inbound, "bob asks"); err != nil {
		t.Fatalf("AppendMessage: %v", err)
	}

	// An empty conversation (every page-load mints one) must not show up.
	if _, err := store.CreateSession(ctx, "", "Alice", "alice@example.test", alice); err != nil {
		t.Fatalf("CreateSession: %v", err)
	}

	summaries, err := store.ListConversations(ctx, alice)
	if err != nil {
		t.Fatalf("ListConversations: %v", err)
	}
	if len(summaries) != 1 {
		t.Fatalf("alice sees %d conversations, want exactly her own non-empty one: %+v", len(summaries), summaries)
	}
	got := summaries[0]
	if got.ConversationID != aliceConv.ConversationID {
		t.Fatalf("conversation id = %q, want %q", got.ConversationID, aliceConv.ConversationID)
	}
	if got.MessageCount != 2 {
		t.Fatalf("MessageCount = %d, want 2", got.MessageCount)
	}
	if got.FirstInbound != "alice asks" {
		t.Fatalf("FirstInbound = %q, want the first INBOUND text", got.FirstInbound)
	}
	if got.UpdatedAt.IsZero() {
		t.Fatal("UpdatedAt was not persisted")
	}

	bobSummaries, err := store.ListConversations(ctx, bob)
	if err != nil {
		t.Fatalf("ListConversations: %v", err)
	}
	if len(bobSummaries) != 1 || bobSummaries[0].ConversationID != bobConv.ConversationID {
		t.Fatalf("BREACH: bob sees %+v, want only his own", bobSummaries)
	}
}

// The auth-disabled (single-tenant local) flavor: Unscoped sees every conversation in
// its org regardless of owner. This is the path a laptop actually runs on, so it gets
// its own test rather than riding on the scoped one — and it still must not reach
// across orgs, since Unscoped widens ownership, not tenancy.
func TestPostgresStoreUnscopedSeesEveryConversationInItsOrg(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()

	org := "org-" + uuid.NewString()
	owned := ConversationScope{Email: "alice@example.test", OrgID: org}
	anonymous := ConversationScope{Unscoped: true, OrgID: org}
	otherOrg := ConversationScope{Unscoped: true, OrgID: "org-other-" + uuid.NewString()}

	for _, scope := range []ConversationScope{owned, anonymous} {
		session, err := store.CreateSession(ctx, "", "U", "", scope)
		if err != nil {
			t.Fatalf("CreateSession: %v", err)
		}
		if _, err := store.AppendMessage(ctx, session.ConversationID, Inbound, "hi"); err != nil {
			t.Fatalf("AppendMessage: %v", err)
		}
	}

	summaries, err := store.ListConversations(ctx, anonymous)
	if err != nil {
		t.Fatalf("ListConversations: %v", err)
	}
	if len(summaries) != 2 {
		t.Fatalf("unscoped sees %d conversations, want both (owned + ownerless): %+v", len(summaries), summaries)
	}

	// Unscoped is not cross-tenant.
	elsewhere, err := store.ListConversations(ctx, otherOrg)
	if err != nil {
		t.Fatalf("ListConversations: %v", err)
	}
	if len(elsewhere) != 0 {
		t.Fatalf("BREACH: an unscoped connection in another org sees %+v", elsewhere)
	}
}

// Org is the OUTER scope: another org's conversation is invisible to list, and
// unresumable — reported identically to one that never existed.
func TestPostgresStoreIsolatesOrganizations(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()

	// SAME email in two orgs — so only the org can be doing the isolating.
	orgA := ConversationScope{Email: "shared@example.test", OrgID: "org-a-" + uuid.NewString()}
	orgB := ConversationScope{Email: "shared@example.test", OrgID: "org-b-" + uuid.NewString()}

	inA, err := store.CreateSession(ctx, "", "Shared", "shared@example.test", orgA)
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	if _, err := store.AppendMessage(ctx, inA.ConversationID, Inbound, "org A only"); err != nil {
		t.Fatalf("AppendMessage: %v", err)
	}

	fromB, err := store.ListConversations(ctx, orgB)
	if err != nil {
		t.Fatalf("ListConversations: %v", err)
	}
	if len(fromB) != 0 {
		t.Fatalf("BREACH: org B sees org A's conversations: %+v", fromB)
	}

	crossOrg, resumed, err := store.ResumeSession(ctx, "", "Shared", "shared@example.test", orgB, inA.ConversationID)
	if err != nil {
		t.Fatalf("ResumeSession: %v", err)
	}
	if resumed || crossOrg.ConversationID == inA.ConversationID {
		t.Fatalf("BREACH: org B resumed org A's conversation (resumed=%v)", resumed)
	}
}

// The workflow step and the OTP-verified bit survive a reconnect, and both are a
// no-op for an unknown session (never an error).
func TestPostgresStorePersistsWorkflowStepAndOtpBit(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()

	session, err := store.CreateSession(ctx, "", "Alice", "alice@example.test", pgScope(t, "alice@example.test"))
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	if err := store.SetCurrentStep(ctx, session.SessionID, "collect-email"); err != nil {
		t.Fatalf("SetCurrentStep: %v", err)
	}
	if err := store.SetSessionAuthenticated(ctx, session.SessionID, true); err != nil {
		t.Fatalf("SetSessionAuthenticated: %v", err)
	}

	fetched, err := newPostgresStore(t).GetSession(ctx, session.SessionID)
	if err != nil || fetched == nil {
		t.Fatalf("GetSession: %v (session %v)", err, fetched)
	}
	if fetched.CurrentStepID != "collect-email" {
		t.Fatalf("CurrentStepID = %q, want collect-email", fetched.CurrentStepID)
	}
	if !fetched.OtpVerified {
		t.Fatal("OtpVerified did not persist")
	}
	// The second write must not have clobbered the first, nor the contact email.
	if fetched.ContactEmail != "alice@example.test" {
		t.Fatalf("ContactEmail = %q — a metadata write clobbered a sibling key", fetched.ContactEmail)
	}

	if err := store.SetCurrentStep(ctx, "unknown-session", "whatever"); err != nil {
		t.Fatalf("SetCurrentStep(unknown) must be a no-op, got %v", err)
	}
	if err := store.SetSessionAuthenticated(ctx, "unknown-session", true); err != nil {
		t.Fatalf("SetSessionAuthenticated(unknown) must be a no-op, got %v", err)
	}
}

// The durable store must report the conversation's ORG on the session, not just the
// owner. ConversationScope.Allows treats an empty org as "unrecorded" and falls
// through to an ownership-only check — so a store that drops it reopens the cross-org
// hole for ownerless conversations while every existing test still passes. Asserted
// against the real gate, not just the field, so it fails if either side regresses.
func TestPostgresStoreReportsOwnerOrgSoTheGateCanEnforceIt(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()

	orgA := "org-a-" + uuid.NewString()
	orgB := "org-b-" + uuid.NewString()

	// An OWNERLESS conversation: ownership alone cannot block a cross-org read, so
	// only the org check can. An owned one would pass this test for the wrong reason.
	anonymous := ConversationScope{Unscoped: true, OrgID: orgA}
	created, err := store.CreateSession(ctx, "", "", "", anonymous)
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	if created.OwnerOrg != orgA {
		t.Fatalf("CreateSession OwnerOrg = %q, want %q", created.OwnerOrg, orgA)
	}
	if created.OwnerEmail != "" {
		t.Fatalf("OwnerEmail = %q, want ownerless", created.OwnerEmail)
	}

	fetched, err := store.GetSession(ctx, created.SessionID)
	if err != nil || fetched == nil {
		t.Fatalf("GetSession: %v (session %v)", err, fetched)
	}
	if fetched.OwnerOrg != orgA {
		t.Fatalf("GetSession OwnerOrg = %q, want %q — the gate cannot enforce an org it is never told", fetched.OwnerOrg, orgA)
	}

	// The gate itself: org B must be refused, org A allowed.
	if (ConversationScope{Email: "someone@example.test", OrgID: orgB}).Allows(fetched.OwnerEmail, fetched.OwnerOrg) {
		t.Fatal("BREACH: another org reached an ownerless conversation through a session id")
	}
	if !(ConversationScope{Email: "someone@example.test", OrgID: orgA}).Allows(fetched.OwnerEmail, fetched.OwnerOrg) {
		t.Fatal("same-org ownerless conversation must stay reachable (th-909995)")
	}
}

// ── admin stores ────────────────────────────────────────────────────────────

func TestPostgresStoreConnectorsAreOrgScoped(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()
	orgA, orgB := "org-a-"+uuid.NewString(), "org-b-"+uuid.NewString()
	now := time.Now().UTC().Truncate(time.Millisecond)

	connector := &connectorConfig{
		ID: uuid.NewString(), Name: "zendesk", Kind: "helpdesk",
		Config: map[string]any{"subdomain": "acme"}, Enabled: true,
		CreatedAt: now, UpdatedAt: now, orgID: orgA,
	}
	if err := store.PutConnector(ctx, connector); err != nil {
		t.Fatalf("PutConnector: %v", err)
	}
	// A second connector, to prove list ordering is by name.
	if err := store.PutConnector(ctx, &connectorConfig{
		ID: uuid.NewString(), Name: "algolia", Kind: "search",
		Config: map[string]any{}, Enabled: false, CreatedAt: now, UpdatedAt: now, orgID: orgA,
	}); err != nil {
		t.Fatalf("PutConnector: %v", err)
	}

	// Read back through a fresh connection — durability, not a process-local map.
	reopened := newPostgresStore(t)
	list, err := reopened.ListConnectors(ctx, orgA)
	if err != nil {
		t.Fatalf("ListConnectors: %v", err)
	}
	if len(list) != 2 || list[0].Name != "algolia" || list[1].Name != "zendesk" {
		t.Fatalf("ListConnectors = %+v, want [algolia zendesk]", list)
	}
	if got := list[1].Config["subdomain"]; got != "acme" {
		t.Fatalf("config did not round-trip: %v", list[1].Config)
	}
	if !list[1].Enabled {
		t.Fatal("enabled did not round-trip")
	}

	// Org B sees nothing, and a cross-org id is reported exactly like an unknown one.
	if empty, err := reopened.ListConnectors(ctx, orgB); err != nil || len(empty) != 0 {
		t.Fatalf("BREACH: org B sees %+v (err %v)", empty, err)
	}
	crossOrg, err := reopened.GetConnector(ctx, orgB, connector.ID)
	if err != nil || crossOrg != nil {
		t.Fatalf("BREACH: cross-org GetConnector = (%v, %v), want (nil, nil)", crossOrg, err)
	}
	unknown, err := reopened.GetConnector(ctx, orgB, uuid.NewString())
	if err != nil || unknown != nil {
		t.Fatalf("GetConnector(unknown) = (%v, %v), want (nil, nil)", unknown, err)
	}
	if deleted, err := reopened.DeleteConnector(ctx, orgB, connector.ID); err != nil || deleted {
		t.Fatalf("BREACH: org B deleted org A's connector (deleted=%v, err=%v)", deleted, err)
	}

	// Upsert updates in place rather than duplicating.
	connector.Name, connector.Enabled = "zendesk-eu", false
	if err := reopened.PutConnector(ctx, connector); err != nil {
		t.Fatalf("PutConnector(update): %v", err)
	}
	updated, err := reopened.GetConnector(ctx, orgA, connector.ID)
	if err != nil || updated == nil {
		t.Fatalf("GetConnector: %v (connector %v)", err, updated)
	}
	if updated.Name != "zendesk-eu" || updated.Enabled {
		t.Fatalf("update did not apply: %+v", updated)
	}
	if after, err := reopened.ListConnectors(ctx, orgA); err != nil || len(after) != 2 {
		t.Fatalf("upsert duplicated a row: %d connectors (err %v)", len(after), err)
	}

	if deleted, err := reopened.DeleteConnector(ctx, orgA, connector.ID); err != nil || !deleted {
		t.Fatalf("DeleteConnector = (%v, %v), want (true, nil)", deleted, err)
	}
	if gone, err := reopened.GetConnector(ctx, orgA, connector.ID); err != nil || gone != nil {
		t.Fatalf("connector survived deletion: (%v, %v)", gone, err)
	}
}

func TestPostgresStoreSettingsRoundTrip(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()
	org := "org-" + uuid.NewString()

	// Unset org reports nil so the handler can substitute defaults.
	unset, err := store.GetSettings(ctx, org)
	if err != nil || unset != nil {
		t.Fatalf("GetSettings(unset) = (%v, %v), want (nil, nil)", unset, err)
	}

	written := &agentSettings{
		OrgID: org, Model: "claude-haiku-4-5", SystemPrompt: "be brief",
		DefaultTools: []string{"search", "email"}, UpdatedAt: time.Now().UTC().Truncate(time.Millisecond),
	}
	if err := store.PutSettings(ctx, written); err != nil {
		t.Fatalf("PutSettings: %v", err)
	}

	read, err := newPostgresStore(t).GetSettings(ctx, org)
	if err != nil || read == nil {
		t.Fatalf("GetSettings: %v (settings %v)", err, read)
	}
	if read.Model != written.Model || read.SystemPrompt != written.SystemPrompt ||
		len(read.DefaultTools) != 2 || read.DefaultTools[0] != "search" || read.DefaultTools[1] != "email" {
		t.Fatalf("settings round-trip mismatch:\n got %+v\nwant %+v", *read, *written)
	}

	// One row per org: a second put replaces rather than duplicating.
	written.Model = "claude-sonnet-5"
	if err := store.PutSettings(ctx, written); err != nil {
		t.Fatalf("PutSettings(update): %v", err)
	}
	updated, err := store.GetSettings(ctx, org)
	if err != nil || updated == nil || updated.Model != "claude-sonnet-5" {
		t.Fatalf("settings update did not apply: %v (err %v)", updated, err)
	}

	// Another org is unaffected.
	if other, err := store.GetSettings(ctx, "org-"+uuid.NewString()); err != nil || other != nil {
		t.Fatalf("BREACH: settings leaked across orgs: (%v, %v)", other, err)
	}
}

func TestPostgresStoreIndexingRunsAreOrgScoped(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()
	orgA, orgB := "org-a-"+uuid.NewString(), "org-b-"+uuid.NewString()
	started := time.Now().UTC().Truncate(time.Millisecond)
	finished := started.Add(time.Second)

	run := &indexingRun{
		ID: uuid.NewString(), ConnectorName: "zendesk", Status: "succeeded",
		StartedAt: started, FinishedAt: &finished, DocumentsSeen: 7, ChunksIndexed: 21,
		DocumentsSkipped: 1, orgID: orgA,
	}
	if err := store.RecordRun(ctx, run); err != nil {
		t.Fatalf("RecordRun: %v", err)
	}
	if err := store.RecordRun(ctx, &indexingRun{
		ID: uuid.NewString(), ConnectorName: "algolia", Status: "failed",
		StartedAt: started.Add(time.Minute), orgID: orgB,
	}); err != nil {
		t.Fatalf("RecordRun: %v", err)
	}

	runs, err := newPostgresStore(t).ListRuns(ctx, orgA)
	if err != nil {
		t.Fatalf("ListRuns: %v", err)
	}
	if len(runs) != 1 {
		t.Fatalf("org A sees %d runs, want 1: %+v", len(runs), runs)
	}
	got := runs[0]
	if got.ID != run.ID || got.ConnectorName != "zendesk" || got.Status != "succeeded" ||
		got.DocumentsSeen != 7 || got.ChunksIndexed != 21 || got.DocumentsSkipped != 1 {
		t.Fatalf("run round-trip mismatch:\n got %+v\nwant %+v", *got, *run)
	}
	if got.FinishedAt == nil || !got.FinishedAt.Equal(finished) {
		t.Fatalf("FinishedAt = %v, want %v", got.FinishedAt, finished)
	}
	if got.Error != nil {
		t.Fatalf("Error = %v, want nil", *got.Error)
	}

	// Re-recording the same id updates in place.
	run.Status = "failed"
	message := "boom"
	run.Error = &message
	if err := store.RecordRun(ctx, run); err != nil {
		t.Fatalf("RecordRun(update): %v", err)
	}
	after, err := store.ListRuns(ctx, orgA)
	if err != nil || len(after) != 1 {
		t.Fatalf("re-record duplicated a run: %d runs (err %v)", len(after), err)
	}
	if after[0].Status != "failed" || after[0].Error == nil || *after[0].Error != "boom" {
		t.Fatalf("run update did not apply: %+v", *after[0])
	}
}

// ── memory stays the default ────────────────────────────────────────────────

// The guard on the whole swap: with SMOOTH_AGENT_STORAGE unset (or memory) nothing
// durable is installed and a server boots with the in-memory stores it always had.
// Needs no Docker.
func TestMemoryStaysDefault(t *testing.T) {
	ctx := t.Context()

	for _, value := range []string{"", "memory"} {
		t.Setenv("SMOOTH_AGENT_STORAGE", value)
		opts, err := StorageOptionsFromEnv(ctx)
		if err != nil {
			t.Fatalf("StorageOptionsFromEnv(%q) = %v, want no error", value, err)
		}
		if opts != nil {
			t.Fatalf("StorageOptionsFromEnv(%q) installed %d options, want none", value, len(opts))
		}
	}

	srv := New()
	if _, ok := srv.store.(*InMemorySessionStore); !ok {
		t.Fatalf("default session store is %T, want *InMemorySessionStore", srv.store)
	}
	if _, ok := srv.admin.(*inMemoryAdminStore); !ok {
		t.Fatalf("default admin store is %T, want *inMemoryAdminStore", srv.admin)
	}
}

// A durable backend that cannot be configured is fatal, never a silent fall back to
// memory — losing durability quietly is the failure worth shouting about.
func TestStorageOptionsRejectMisconfiguration(t *testing.T) {
	ctx := t.Context()

	t.Setenv("SMOOTH_AGENT_STORAGE", "postgres")
	t.Setenv("SMOOTH_AGENT_DATABASE_URL", "")
	t.Setenv("DATABASE_URL", "")
	if _, err := StorageOptionsFromEnv(ctx); err == nil {
		t.Fatal("postgres without a database URL must be an error, not a silent memory fallback")
	}

	t.Setenv("SMOOTH_AGENT_STORAGE", "cassandra")
	if _, err := StorageOptionsFromEnv(ctx); err == nil {
		t.Fatal("an unknown storage backend must be an error")
	}
}

func texts(messages []StoredMessage) []string {
	out := make([]string, len(messages))
	for i, m := range messages {
		out[i] = m.Text
	}
	return out
}

// ── schema integrity (th-5a5181 P2) ─────────────────────────────────────────

// The json columns are NOT NULL DEFAULT '{}', so "absent" has ONE representation on
// read instead of two.
//
// These inserts OMIT the json columns rather than passing an explicit NULL, so the
// DEFAULT fires on its own — no coalesce needed here, unlike the Rust adapter whose
// inserts name every column. This test is what fails if either half regresses: drop the
// NOT NULL DEFAULT and these read back NULL; start passing an explicit NULL and the
// insert dies on the not-null constraint.
func TestPostgresStoreAbsentJSONReadsBackAsAnEmptyObject(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()
	scope := pgScope(t, "alice@example.test")

	created, err := store.CreateSession(ctx, "", "Alice", "alice@example.test", scope)
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	if _, err := store.AppendMessage(ctx, created.ConversationID, Inbound, "hello"); err != nil {
		t.Fatalf("AppendMessage: %v", err)
	}

	var metaJSON, analyticsJSON string
	if err := store.pool.QueryRow(ctx,
		`SELECT metadata_json::text, analytics_json::text FROM conversations WHERE id = $1`,
		created.ConversationID).Scan(&metaJSON, &analyticsJSON); err != nil {
		t.Fatalf("read conversation: %v", err)
	}
	if metaJSON != "{}" || analyticsJSON != "{}" {
		t.Errorf("conversation json = %q/%q, want {}/{}", metaJSON, analyticsJSON)
	}

	if err := store.pool.QueryRow(ctx,
		`SELECT metadata_json::text, analytics_json::text FROM conversation_messages WHERE conversation_id = $1`,
		created.ConversationID).Scan(&metaJSON, &analyticsJSON); err != nil {
		t.Fatalf("read message: %v", err)
	}
	if metaJSON != "{}" || analyticsJSON != "{}" {
		t.Errorf("message json = %q/%q, want {}/{}", metaJSON, analyticsJSON)
	}

	if err := store.pool.QueryRow(ctx,
		`SELECT metadata_json::text FROM conversation_participants WHERE conversation_id = $1 LIMIT 1`,
		created.ConversationID).Scan(&metaJSON); err != nil {
		t.Fatalf("read participant: %v", err)
	}
	if metaJSON != "{}" {
		t.Errorf("participant metadata_json = %q, want {}", metaJSON)
	}

	// status passes the new CHECK, and the three session timestamps are non-null.
	var status string
	var createdAt, updatedAt, lastActivityAt time.Time
	if err := store.pool.QueryRow(ctx,
		`SELECT status, created_at, updated_at, last_activity_at FROM conversation_sessions WHERE session_id = $1`,
		created.SessionID).Scan(&status, &createdAt, &updatedAt, &lastActivityAt); err != nil {
		t.Fatalf("read session: %v", err)
	}
	if status != "active" {
		t.Errorf("status = %q, want active", status)
	}
	if createdAt.IsZero() || updatedAt.IsZero() || lastActivityAt.IsZero() {
		t.Error("session timestamps must all be set")
	}
}

// The CHECK is what stops a typo'd platform reaching the table at all.
func TestPostgresStorePlatformCheckRejectsAnUnknownValue(t *testing.T) {
	store := newPostgresStore(t)
	_, err := store.pool.Exec(t.Context(),
		`INSERT INTO conversations (id, platform, name, organization_id, idempotency_key)
		 VALUES ($1, 'carrier-pigeon', '', $2, $1)`,
		uuid.NewString(), "org-"+uuid.NewString())
	if err == nil {
		t.Fatal("an unknown platform must violate the CHECK")
	}
	if !strings.Contains(err.Error(), "conversations_platform_check") {
		t.Errorf("want a platform CHECK violation, got: %v", err)
	}
}

// ── agentless sessions (th-68897a) ──────────────────────────────────────────

// A session created with no agentId has NO agent. Both stores used to mint a fresh
// UUID here, which pointed every agentless session at an agent that never existed —
// invisible until something tried to resolve it. Covers BOTH stores: the fabrication
// lived in each, so testing one would leave the other broken.
func TestSessionWithNoAgentHasNoAgent(t *testing.T) {
	ctx := t.Context()

	mem := NewInMemorySessionStore()
	for _, in := range []string{"", "   "} {
		created, err := mem.CreateSession(ctx, in, "Alice", "", ConversationScope{})
		if err != nil {
			t.Fatalf("in-memory CreateSession(%q): %v", in, err)
		}
		if created.AgentID != "" {
			t.Errorf("in-memory agentId for %q = %q, want empty", in, created.AgentID)
		}
	}

	store := newPostgresStore(t)
	scope := pgScope(t, "alice@example.test")
	created, err := store.CreateSession(ctx, "   ", "Alice", "alice@example.test", scope)
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	if created.AgentID != "" {
		t.Errorf("agentId = %q, want empty", created.AgentID)
	}

	// The column itself is NULL, not an empty string standing in for one.
	var agentID *string
	if err := store.pool.QueryRow(ctx,
		`SELECT agent_id FROM conversation_sessions WHERE session_id = $1`,
		created.SessionID).Scan(&agentID); err != nil {
		t.Fatalf("read agent_id: %v", err)
	}
	if agentID != nil {
		t.Errorf("agent_id = %q, want NULL", *agentID)
	}

	// …and it survives the round trip rather than coming back as a uuid.
	fetched, err := store.GetSession(ctx, created.SessionID)
	if err != nil || fetched == nil {
		t.Fatalf("GetSession: %v", err)
	}
	if fetched.AgentID != "" {
		t.Errorf("round-tripped agentId = %q, want empty", fetched.AgentID)
	}
}

// The declared render capabilities ride the CONVERSATION, so the session a reconnect
// mints inherits them — the durable half of th-13df6d. Also asserts the edge the
// in-memory store gets for free but SQL does not: an empty list CLEARS the stored key
// rather than leaving a stale set for the next omitting reconnect to resurrect.
func TestPostgresStorePersistsConversationSupports(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()
	scope := pgScope(t, "alice@example.test")

	session, err := store.CreateSession(ctx, "", "Alice", "alice@example.test", scope)
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	if err := store.SetConversationSupports(ctx, session.ConversationID, []string{"identity_form"}); err != nil {
		t.Fatalf("SetConversationSupports: %v", err)
	}

	// A reconnect: a NEW session on the same conversation, through a fresh store.
	reconnected, resumed, err := newPostgresStore(t).ResumeSession(ctx, "", "Alice", "alice@example.test", scope, session.ConversationID)
	if err != nil || !resumed {
		t.Fatalf("ResumeSession: resumed=%v err=%v", resumed, err)
	}
	if !capabilitySet(reconnected.Supports)["identity_form"] {
		t.Fatalf("resumed session lost the conversation's capabilities: %v", reconnected.Supports)
	}
	// …and the turn's read path (GetSession) reports the same set.
	fetched, err := store.GetSession(ctx, reconnected.SessionID)
	if err != nil || fetched == nil {
		t.Fatalf("GetSession: %v (session %v)", err, fetched)
	}
	if !capabilitySet(fetched.Supports)["identity_form"] {
		t.Fatalf("GetSession reported capabilities %v, want identity_form", fetched.Supports)
	}

	// The text-only opt-out clears the key — a later resume must not resurrect it.
	if err := store.SetConversationSupports(ctx, session.ConversationID, nil); err != nil {
		t.Fatalf("SetConversationSupports(empty): %v", err)
	}
	afterOptOut, _, err := store.ResumeSession(ctx, "", "Alice", "alice@example.test", scope, session.ConversationID)
	if err != nil {
		t.Fatalf("ResumeSession after opt-out: %v", err)
	}
	if len(afterOptOut.Supports) != 0 {
		t.Fatalf("the opt-out left a stale set: %v", afterOptOut.Supports)
	}

	// Unknown conversation: a no-op, never an error.
	if err := store.SetConversationSupports(ctx, "unknown-conversation", []string{"identity_form"}); err != nil {
		t.Fatalf("SetConversationSupports(unknown) must be a no-op, got %v", err)
	}
}
