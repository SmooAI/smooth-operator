package server

// list_conversations paging + search (SMOODEV-3744) — the Go port of
// rust/smooth-operator-server/tests/list_conversations_paging.rs and the
// ConversationKey / ConversationSummaryQuery unit tests at the bottom of
// rust/smooth-operator/src/adapter.rs.
//
// The sidebar used to return only the newest `limit` rows with no way to reach
// older ones, and a client-side search could only filter what it had loaded. These
// pin the keyset `cursor` / `nextCursor` and the server-side `query`:
//
//   - pages are disjoint and, together, exactly the unpaged listing;
//   - ties on updatedAt are broken by id, so equal timestamps neither repeat nor
//     vanish across a page boundary;
//   - a conversation bumped mid-paging is never returned twice, and every untouched
//     one is still returned (the bumped one moves above the cursor — documented
//     keyset behaviour);
//   - a search narrows the caller's scope and never widens it: another member's
//     chat and a foreign org's chat that match are still excluded.
//
// Go has ONE listing path (the Rust "scan" flavor: ConversationScope.Allows), under
// which an ownerless chat in the caller's org IS visible (th-909995) — so the world
// below includes one, and the expectations account for it.

import (
	"context"
	"encoding/base64"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/SmooAI/smooth-operator/go/protocol"
)

const (
	pagingOrg = "org-alpha"
	pagingMe  = "brent@smoo.ai"
)

// pagingWorld is one seeded store plus the ids the assertions need.
type pagingWorld struct {
	store *InMemorySessionStore
	// visible is everything pagingMe may list, in sidebar order (newest first, ties
	// by id descending): seven of mine plus the ownerless widget chat.
	visible []string
	// mine is my seven, in sidebar order.
	mine    []string
	acme    string // the one of mine whose first message mentions Acme
	theirs  string // another member's chat that matches "acme"
	machine string // an ownerless chat in my org that matches "acme"
	foreign string // an ownerless chat in ANOTHER org that matches "acme"
}

// seedConversation adds a non-empty conversation owned by owner ("" = ownerless) in
// org, whose first inbound message is first, then pins its last activity to ts.
func seedPagingConversation(t *testing.T, store *InMemorySessionStore, ts time.Time, owner, org, first string) string {
	t.Helper()
	ctx := context.Background()
	session, err := store.CreateSession(ctx, "agent", "U", owner, ConversationScope{Email: owner, OrgID: org})
	if err != nil {
		t.Fatalf("create session: %v", err)
	}
	if _, err := store.AppendMessage(ctx, session.ConversationID, Inbound, first); err != nil {
		t.Fatalf("append: %v", err)
	}
	store.mu.Lock()
	store.updatedAt[session.ConversationID] = ts
	store.mu.Unlock()
	return session.ConversationID
}

func newPagingWorld(t *testing.T) pagingWorld {
	t.Helper()
	now := time.Now()
	ago := func(secs int) time.Time { return now.Add(-time.Duration(secs) * time.Second) }
	store := NewInMemorySessionStore()
	w := pagingWorld{store: store}
	keys := map[string]time.Time{}

	// Seven of mine; three of them share one timestamp to force the id tiebreak
	// across a page boundary.
	tied := ago(400)
	for i, ts := range []time.Time{ago(10), ago(20), tied, tied, tied, ago(500), ago(600)} {
		first := "status update"
		if i == 5 {
			first = "Where is the Acme invoice?"
		}
		id := seedPagingConversation(t, store, ts, pagingMe, pagingOrg, first)
		if i == 5 {
			w.acme = id
		}
		w.mine = append(w.mine, id)
		keys[id] = ts
	}
	w.theirs = seedPagingConversation(t, store, ago(15), "tara@smoo.ai", pagingOrg, "acme renewal")
	w.machine = seedPagingConversation(t, store, ago(25), "", pagingOrg, "ACME widget chat")
	keys[w.machine] = ago(25)
	w.foreign = seedPagingConversation(t, store, ago(12), "", "org-beta", "acme from elsewhere")

	byKey := func(ids []string) []string {
		out := slices.Clone(ids)
		slices.SortFunc(out, func(a, b string) int {
			if c := keys[b].Compare(keys[a]); c != 0 {
				return c
			}
			return strings.Compare(b, a)
		})
		return out
	}
	w.mine = byKey(w.mine)
	w.visible = byKey(append(slices.Clone(w.mine), w.machine))
	return w
}

// pagingAccess is pagingMe signed in to an auth-enabled server.
func pagingAccess() AccessContext {
	return AccessContext{Principal: Principal{Sub: pagingMe, Org: pagingOrg, Email: pagingMe}, AuthEnabled: true}
}

func pagingDispatcher(store SessionStore, access AccessContext) *FrameDispatcher {
	return NewFrameDispatcher(store, nil, access, "", nil, nil, nil, nil, nil, "", nil, nil, nil, nil)
}

// listPage dispatches one list_conversations frame and returns its single event.
func listPage(t *testing.T, d *FrameDispatcher, args map[string]any) map[string]any {
	t.Helper()
	frame := map[string]any{"action": "list_conversations", "requestId": "lc"}
	for k, v := range args {
		frame[k] = v
	}
	sink, events := capture()
	dispatchJSON(t, d, frame, sink)
	if len(*events) != 1 {
		t.Fatalf("want exactly one event, got %d: %+v", len(*events), *events)
	}
	return (*events)[0]
}

func pageData(t *testing.T, ev map[string]any) map[string]any {
	t.Helper()
	if ev["type"] != "immediate_response" {
		t.Fatalf("want immediate_response, got %+v", ev)
	}
	return ev["data"].(map[string]any)
}

func pageIDs(t *testing.T, ev map[string]any) []string {
	t.Helper()
	ids := []string{}
	for _, row := range pageData(t, ev)["conversations"].([]map[string]any) {
		ids = append(ids, row["conversationId"].(string))
	}
	return ids
}

// nextCursorOf returns the page's nextCursor ("" when null), asserting it is present
// as an explicit null rather than absent, and that it agrees with hasMore.
func nextCursorOf(t *testing.T, ev map[string]any) string {
	t.Helper()
	data := pageData(t, ev)
	raw, present := data["nextCursor"]
	if !present {
		t.Fatalf("nextCursor must be present (explicit null on the last page): %+v", data)
	}
	hasMore, ok := data["hasMore"].(bool)
	if !ok {
		t.Fatalf("hasMore missing or not a bool: %+v", data)
	}
	if raw == nil {
		if hasMore {
			t.Fatalf("no nextCursor means no more: %+v", data)
		}
		return ""
	}
	cursor, ok := raw.(string)
	if !ok || cursor == "" || !hasMore {
		t.Fatalf("a nextCursor implies hasMore: %+v", data)
	}
	return cursor
}

// allPages pages through everything with limit (and query, when non-empty).
func allPages(t *testing.T, d *FrameDispatcher, limit int, query string) [][]string {
	t.Helper()
	var pages [][]string
	cursor := ""
	for {
		args := map[string]any{"limit": limit}
		if cursor != "" {
			args["cursor"] = cursor
		}
		if query != "" {
			args["query"] = query
		}
		ev := listPage(t, d, args)
		pages = append(pages, pageIDs(t, ev))
		if cursor = nextCursorOf(t, ev); cursor == "" {
			return pages
		}
		if len(pages) > 50 {
			t.Fatal("paging never terminated")
		}
	}
}

func pageSizes(pages [][]string) []int {
	out := []int{}
	for _, p := range pages {
		out = append(out, len(p))
	}
	return out
}

// Rust: owned_pages_are_disjoint_and_complete / scan_pages_are_disjoint_and_complete.
// Every page size from 1 to 4 — at 2 and 3 the three-way timestamp tie straddles a
// page boundary.
func TestListConversationsPagesAreDisjointAndComplete(t *testing.T) {
	w := newPagingWorld(t)
	d := pagingDispatcher(w.store, pagingAccess())

	unpaged := pageIDs(t, listPage(t, d, map[string]any{"limit": 100}))
	if !slices.Equal(unpaged, w.visible) {
		t.Fatalf("unpaged listing = %v, want %v", unpaged, w.visible)
	}
	for limit := 1; limit <= 4; limit++ {
		pages := allPages(t, d, limit, "")
		if got := slices.Concat(pages...); !slices.Equal(got, w.visible) {
			t.Fatalf("limit %d: pages concatenate to %v, want %v (sizes %v)", limit, got, w.visible, pageSizes(pages))
		}
	}
	// Scoping runs before paging: never another member's chat, never another org's.
	all := slices.Concat(allPages(t, d, 2, "")...)
	if slices.Contains(all, w.theirs) || slices.Contains(all, w.foreign) {
		t.Fatalf("BREACH: listing leaked another member's / org's chat: %v", all)
	}
}

// Rust: a_page_the_size_of_the_rest_reports_no_more.
func TestListConversationsAPageTheSizeOfTheRestReportsNoMore(t *testing.T) {
	w := newPagingWorld(t)
	ev := listPage(t, pagingDispatcher(w.store, pagingAccess()), map[string]any{"limit": len(w.visible)})
	if got := pageIDs(t, ev); !slices.Equal(got, w.visible) {
		t.Fatalf("got %v, want %v", got, w.visible)
	}
	if nextCursorOf(t, ev) != "" || pageData(t, ev)["hasMore"] != false {
		t.Fatalf("an exact-size last page must report hasMore=false, nextCursor=null: %+v", ev)
	}
}

// Rust: no_cursor_is_the_old_first_page.
func TestListConversationsNoCursorIsTheOldFirstPage(t *testing.T) {
	w := newPagingWorld(t)
	d := pagingDispatcher(w.store, pagingAccess())
	if got := pageIDs(t, listPage(t, d, map[string]any{})); !slices.Equal(got, w.visible) {
		t.Fatalf("default limit 50 should return every visible row: got %v", got)
	}
	got := pageIDs(t, listPage(t, d, map[string]any{"limit": 2, "cursor": "", "query": "   "}))
	if !slices.Equal(got, w.visible[:2]) {
		t.Fatalf("a blank cursor is no cursor and a blank query no filter: got %v, want %v", got, w.visible[:2])
	}
}

// Rust: owned_paging_survives_concurrent_updates (bump_between_pages). Bump one row on
// the NEXT page and one on the page already read, between pages. Neither may be
// returned twice; every untouched row must still come back; the bumped next-page row
// moves above the cursor and is not returned by the remaining pages (it heads a fresh
// first page instead).
func TestListConversationsPagingSurvivesConcurrentUpdates(t *testing.T) {
	w := newPagingWorld(t)
	d := pagingDispatcher(w.store, pagingAccess())

	first := listPage(t, d, map[string]any{"limit": 3})
	page1 := pageIDs(t, first)
	cursor := nextCursorOf(t, first)
	if cursor == "" {
		t.Fatal("want more pages after the first")
	}

	bumpedAhead := w.visible[4] // would have been on page 2
	bumpedBehind := page1[1]    // already returned
	for _, id := range []string{bumpedAhead, bumpedBehind} {
		time.Sleep(2 * time.Millisecond)
		if _, err := w.store.AppendMessage(context.Background(), id, Outbound, "bump"); err != nil {
			t.Fatalf("bump: %v", err)
		}
	}

	var rest []string
	for cursor != "" {
		ev := listPage(t, d, map[string]any{"limit": 3, "cursor": cursor})
		rest = append(rest, pageIDs(t, ev)...)
		cursor = nextCursorOf(t, ev)
	}

	seen := slices.Concat(page1, rest)
	dedup := slices.Compact(slices.Sorted(slices.Values(seen)))
	if len(dedup) != len(seen) {
		t.Fatalf("a row was returned twice: %v", seen)
	}
	for _, id := range w.visible {
		if id != bumpedAhead && !slices.Contains(seen, id) {
			t.Fatalf("untouched row %s was dropped: %v", id, seen)
		}
	}
	if slices.Contains(rest, bumpedAhead) {
		t.Fatalf("a row bumped above the cursor must not be on later pages: %v", rest)
	}
	if got := pageIDs(t, listPage(t, d, map[string]any{"limit": 1})); !slices.Equal(got, []string{bumpedBehind}) {
		t.Fatalf("the latest bump heads a fresh first page: got %v, want [%s]", got, bumpedBehind)
	}
}

// Rust: owned_search_matches_my_titles_and_never_widens_scope +
// scan_search_never_returns_another_members_chat. "acme" matches my first message,
// Tara's chat, the ownerless widget chat in my org and one in another org; the search
// returns only what I could list anyway.
func TestListConversationsSearchIsCaseInsensitiveTrimmedAndNeverWidensScope(t *testing.T) {
	w := newPagingWorld(t)
	ev := listPage(t, pagingDispatcher(w.store, pagingAccess()), map[string]any{"query": "  AcMe "})
	if got, want := pageIDs(t, ev), []string{w.machine, w.acme}; !slices.Equal(got, want) {
		t.Fatalf("search = %v, want %v (never Tara's %s or the foreign %s)", got, want, w.theirs, w.foreign)
	}
	if title := pageData(t, ev)["conversations"].([]map[string]any)[1]["title"]; title != "Where is the Acme invoice?" {
		t.Fatalf("title = %v", title)
	}
	if pageData(t, ev)["hasMore"] != false {
		t.Fatalf("hasMore = %v, want false", pageData(t, ev)["hasMore"])
	}
}

// Rust: an_emailless_principal_pages_nothing. Go's scoping differs by design
// (th-909995): an emailless principal still sees its org's OWNERLESS chats, so the
// Go invariant is "only ownerless, never an owned one" rather than "nothing".
func TestListConversationsEmaillessPrincipalSearchSeesOnlyOwnerless(t *testing.T) {
	w := newPagingWorld(t)
	emailless := AccessContext{Principal: Principal{Sub: "svc", Org: pagingOrg}, AuthEnabled: true}
	ev := listPage(t, pagingDispatcher(w.store, emailless), map[string]any{"query": "acme"})
	if got := pageIDs(t, ev); !slices.Equal(got, []string{w.machine}) {
		t.Fatalf("emailless search = %v, want only the ownerless %s", got, w.machine)
	}
	if pageData(t, ev)["hasMore"] != false {
		t.Fatalf("hasMore = %v, want false", pageData(t, ev)["hasMore"])
	}
}

// namedStore decorates the in-memory store's summaries with conversation names — the
// in-memory store has no name concept, but a Postgres row (possibly written by the
// Rust server) does, so the handler must treat a name the way Rust does.
type namedStore struct {
	*InMemorySessionStore
	names map[string]string
}

func (s namedStore) ListConversations(ctx context.Context, scope ConversationScope) ([]ConversationSummary, error) {
	out, err := s.InMemorySessionStore.ListConversations(ctx, scope)
	for i := range out {
		out[i].Name = s.names[out[i].ConversationID]
	}
	return out, err
}

// Rust: search_matches_a_meaningful_name_but_not_the_default_placeholder.
func TestListConversationsSearchMatchesAMeaningfulNameButNotTheDefaultPlaceholder(t *testing.T) {
	w := newPagingWorld(t)
	names := map[string]string{}
	for i, id := range w.visible {
		names[id] = "Session " + string(rune('a'+i))
	}
	names[w.mine[6]] = "Q3 Forecast"
	d := pagingDispatcher(namedStore{w.store, names}, pagingAccess())

	ev := listPage(t, d, map[string]any{"query": "forecast"})
	if got := pageIDs(t, ev); !slices.Equal(got, []string{w.mine[6]}) {
		t.Fatalf("name search = %v, want [%s]", got, w.mine[6])
	}
	if title := pageData(t, ev)["conversations"].([]map[string]any)[0]["title"]; title != "Q3 Forecast" {
		t.Fatalf("a meaningful name is the title: got %v", title)
	}
	// Every other row is named "Session …"; that placeholder is not a title.
	if got := pageIDs(t, listPage(t, d, map[string]any{"query": "session"})); len(got) != 0 {
		t.Fatalf("the default placeholder name must not match: %v", got)
	}
}

// Rust: search_pages_with_a_cursor.
func TestListConversationsSearchPagesWithACursor(t *testing.T) {
	w := newPagingWorld(t)
	pages := allPages(t, pagingDispatcher(w.store, pagingAccess()), 2, "status")
	want := slices.DeleteFunc(slices.Clone(w.mine), func(id string) bool { return id == w.acme })
	if !slices.Equal(pageSizes(pages), []int{2, 2, 2}) {
		t.Fatalf("page sizes = %v, want [2 2 2]", pageSizes(pages))
	}
	if got := slices.Concat(pages...); !slices.Equal(got, want) {
		t.Fatalf("the six 'status update' rows, in order, once each: got %v, want %v", got, want)
	}
}

// Rust: an_unknown_cursor_is_a_validation_error — plus the other ways a cursor can
// fail to be one this server issued (no separator, empty id, bad timestamp).
func TestListConversationsUnknownCursorIsAValidationError(t *testing.T) {
	w := newPagingWorld(t)
	d := pagingDispatcher(w.store, pagingAccess())
	enc := base64.RawURLEncoding.EncodeToString
	for _, cursor := range []string{
		"not-a-cursor",
		"bm9waXBl", // "nopipe"
		enc([]byte("2026-10-08T00:00:00Z|")),
		enc([]byte("yesterday|3333")),
	} {
		ev := listPage(t, d, map[string]any{"cursor": cursor})
		if ev["type"] != "error" {
			t.Fatalf("cursor %q: want an error event, got %+v", cursor, ev)
		}
		if code := ev["data"].(map[string]any)["error"].(map[string]any)["code"]; code != "VALIDATION_ERROR" {
			t.Fatalf("cursor %q: code = %v, want VALIDATION_ERROR", cursor, code)
		}
	}
}

// The page a real server emits validates against the spec's Response schema —
// including the explicit-null nextCursor of a last page.
func TestListConversationsResponseValidatesAgainstSpec(t *testing.T) {
	v, err := protocol.NewValidator(specDir(t))
	if err != nil {
		t.Fatalf("load validator: %v", err)
	}
	w := newPagingWorld(t)
	d := pagingDispatcher(w.store, pagingAccess())
	for _, args := range []map[string]any{{"limit": 3}, {"limit": 100}} {
		ev := listPage(t, d, args)
		if err := v.ValidateRef("actions/list-conversations.schema.json#/$defs/Response", asTree(t, pageData(t, ev))); err != nil {
			t.Fatalf("args %v: response does not match spec: %v", args, err)
		}
		if err := v.ValidateRef("events/immediate-response.schema.json", asTree(t, ev)); err != nil {
			t.Fatalf("args %v: event does not match spec: %v", args, err)
		}
	}
}

// ── conversationKey / search unit tests (Rust: adapter.rs tests) ──────────────

// Rust: cursor_round_trips_at_full_precision.
func TestConversationCursorRoundTripsAtFullPrecision(t *testing.T) {
	for _, k := range []conversationKey{
		{UpdatedAt: time.Unix(1_790_000_000, 123_456_789).UTC(), ID: "3f2a0c4e-0000-4000-8000-000000000001"},
		{UpdatedAt: time.Unix(1_790_000_000, 0).UTC(), ID: "a"},
	} {
		got, ok := decodeConversationKey(k.encode())
		if !ok || !got.UpdatedAt.Equal(k.UpdatedAt) || got.ID != k.ID {
			t.Fatalf("round trip of %+v = %+v, %v", k, got, ok)
		}
	}
}

// The cursor in spec/conformance/fixtures.json (minted by the Rust server) decodes,
// and Go mints the byte-identical cursor for the same key — so a cursor survives a
// client switching between engines.
func TestConversationCursorMatchesTheRustFixture(t *testing.T) {
	const fixture = "MjAyNi0xMC0wOFQxNDozMDowMC4xMjM0NTZafDMzMzMzMzMzLTMzMzMtMzMzMy0zMzMzLTMzMzMzMzMzMzMzMw"
	k, ok := decodeConversationKey(fixture)
	want := conversationKey{UpdatedAt: time.Date(2026, 10, 8, 14, 30, 0, 123_456_000, time.UTC), ID: "33333333-3333-3333-3333-333333333333"}
	if !ok || !k.UpdatedAt.Equal(want.UpdatedAt) || k.ID != want.ID {
		t.Fatalf("decode(fixture) = %+v, %v; want %+v", k, ok, want)
	}
	if got := want.encode(); got != fixture {
		t.Fatalf("encode = %q, want the Rust-minted %q", got, fixture)
	}
	// Any RFC 3339 timestamp decodes — an offset and nanoseconds included.
	offset := base64.RawURLEncoding.EncodeToString([]byte("2026-10-08T16:30:00.123456+02:00|33333333-3333-3333-3333-333333333333"))
	if k, ok := decodeConversationKey(offset); !ok || !k.UpdatedAt.Equal(want.UpdatedAt) {
		t.Fatalf("offset cursor decoded to %+v, %v", k, ok)
	}
}

// Rust: foreign_cursors_do_not_decode.
func TestForeignConversationCursorsDoNotDecode(t *testing.T) {
	for _, bad := range []string{"", "not base64!", "bm9waXBl", "MjAyNi0xMC0wOFQwMDowMDowMFp8"} {
		if k, ok := decodeConversationKey(bad); ok {
			t.Fatalf("decode(%q) = %+v, want failure", bad, k)
		}
	}
}

// Rust: precedes_is_newest_first_then_id_descending.
func TestConversationKeyPrecedesIsNewestFirstThenIDDescending(t *testing.T) {
	at := func(s int64) time.Time { return time.Unix(s, 0).UTC() }
	k := conversationKey{UpdatedAt: at(100), ID: "m"}
	cases := []struct {
		name string
		ts   time.Time
		id   string
		want bool
	}{
		{"older sorts after", at(99), "z", true},
		{"newer sorts before", at(101), "a", false},
		{"same time, smaller id sorts after", at(100), "a", true},
		{"the key itself is not after itself", at(100), "m", false},
		{"same time, larger id sorts before", at(100), "z", false},
	}
	for _, tc := range cases {
		if got := k.precedes(tc.ts, tc.id); got != tc.want {
			t.Errorf("%s: precedes = %v, want %v", tc.name, got, tc.want)
		}
	}
}

// Rust: search_matches_meaningful_name_or_first_message_case_insensitively.
func TestConversationSearchMatchesMeaningfulNameOrFirstMessage(t *testing.T) {
	needle := strings.TrimSpace("  ACME ")
	if !matchesConversationSearch(needle, "Acme renewal", "") {
		t.Error("a meaningful name matches")
	}
	if !matchesConversationSearch(needle, "Session 1", "where is the acme invoice") {
		t.Error("the first inbound message matches")
	}
	if matchesConversationSearch(needle, "Session acme", "") {
		t.Error("the placeholder name is not a title")
	}
	if matchesConversationSearch(needle, "Q3", "nothing here") {
		t.Error("no match")
	}
	if !matchesConversationSearch("", "anything", "") {
		t.Error("a blank search matches everything")
	}
}

// ── Postgres store path ───────────────────────────────────────────────────────

// The same contract through the durable store: ties and 1µs neighbours at Postgres'
// full (microsecond) precision page without repeats or gaps, a Rust-written name is
// searched and titled while the "Session …" placeholder is not, and a search never
// reaches another member's chat. Skips cleanly without Docker, like every Postgres test.
func TestPostgresStoreListConversationsPagesAndSearches(t *testing.T) {
	store := newPostgresStore(t)
	ctx := t.Context()
	me := pgScope(t, pagingMe)
	tara := ConversationScope{Email: "tara@smoo.ai", OrgID: me.OrgID}
	ownerless := ConversationScope{OrgID: me.OrgID}

	base := time.Date(2026, 10, 8, 14, 30, 0, 123_456_000, time.UTC)
	seed := func(scope ConversationScope, ts time.Time, name, first string) string {
		t.Helper()
		session, err := store.CreateSession(ctx, "", "U", scope.Email, scope)
		if err != nil {
			t.Fatalf("CreateSession: %v", err)
		}
		if _, err := store.AppendMessage(ctx, session.ConversationID, Inbound, first); err != nil {
			t.Fatalf("AppendMessage: %v", err)
		}
		if _, err := store.pool.Exec(ctx, `UPDATE conversations SET updated_at = $2, name = $3 WHERE id = $1`,
			session.ConversationID, ts, name); err != nil {
			t.Fatalf("pin updated_at: %v", err)
		}
		return session.ConversationID
	}

	tied := base.Add(-400 * time.Second)
	keys := map[string]time.Time{}
	var visible []string
	for i, ts := range []time.Time{base, base.Add(-time.Microsecond), tied, tied, tied} {
		id := seed(me, ts, "Session "+string(rune('a'+i)), "status update")
		keys[id] = ts
		visible = append(visible, id)
	}
	forecast := seed(me, base.Add(-500*time.Second), "Q3 Forecast", "numbers please")
	keys[forecast] = base.Add(-500 * time.Second)
	machine := seed(ownerless, base.Add(-20*time.Second), "", "ACME widget chat")
	keys[machine] = base.Add(-20 * time.Second)
	visible = append(visible, forecast, machine)
	theirs := seed(tara, base.Add(-15*time.Second), "", "acme renewal")
	slices.SortFunc(visible, func(a, b string) int {
		if c := keys[b].Compare(keys[a]); c != 0 {
			return c
		}
		return strings.Compare(b, a)
	})

	access := AccessContext{Principal: Principal{Sub: pagingMe, Org: me.OrgID, Email: pagingMe}, AuthEnabled: true}
	d := pagingDispatcher(store, access)

	for limit := 1; limit <= 3; limit++ {
		if got := slices.Concat(allPages(t, d, limit, "")...); !slices.Equal(got, visible) {
			t.Fatalf("limit %d: pages concatenate to %v, want %v", limit, got, visible)
		}
	}

	ev := listPage(t, d, map[string]any{"query": "FORECAST"})
	if got := pageIDs(t, ev); !slices.Equal(got, []string{forecast}) {
		t.Fatalf("name search = %v, want [%s]", got, forecast)
	}
	if title := pageData(t, ev)["conversations"].([]map[string]any)[0]["title"]; title != "Q3 Forecast" {
		t.Fatalf("a meaningful name is the title: got %v", title)
	}
	if got := pageIDs(t, listPage(t, d, map[string]any{"query": "session"})); len(got) != 0 {
		t.Fatalf("the default placeholder name must not match: %v", got)
	}
	got := pageIDs(t, listPage(t, d, map[string]any{"query": " acme "}))
	if !slices.Equal(got, []string{machine}) || slices.Contains(got, theirs) {
		t.Fatalf("acme search = %v, want only the ownerless %s (never Tara's %s)", got, machine, theirs)
	}
}

// TestListConversationsALimitOverTheMaximumIsClampedNotRejected: a pre-paging client
// asking for 1000 gets the 200-row maximum and a cursor for the rest, not an error.
// Rust: a_limit_over_the_maximum_is_clamped_not_rejected.
func TestListConversationsALimitOverTheMaximumIsClampedNotRejected(t *testing.T) {
	store := NewInMemorySessionStore()
	now := time.Now()
	for i := range 205 {
		seedPagingConversation(t, store, now.Add(-time.Duration(i)*time.Second), pagingMe, pagingOrg, "status update")
	}
	d := pagingDispatcher(store, pagingAccess())
	first := listPage(t, d, map[string]any{"limit": 1000})
	if got := len(pageIDs(t, first)); got != 200 {
		t.Fatalf("want the 200-row maximum, got %d", got)
	}
	cursor := nextCursorOf(t, first)
	if cursor == "" {
		t.Fatal("a clamped page must still offer the rest")
	}
	rest := listPage(t, d, map[string]any{"limit": 1000, "cursor": cursor})
	if got := len(pageIDs(t, rest)); got != 5 {
		t.Fatalf("want the remaining 5, got %d", got)
	}
	if nextCursorOf(t, rest) != "" {
		t.Fatal("the last page has no cursor")
	}
}
