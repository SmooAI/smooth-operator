using System.Text;
using System.Text.Json.Nodes;
using SmooAI.SmoothOperator;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// <c>list_conversations</c> keyset paging + server-side search (SMOODEV-3744). The C# parity of the
/// Rust <c>rust/smooth-operator-server/tests/list_conversations_paging.rs</c> and the
/// <c>ConversationKey</c> / <c>ConversationSummaryQuery</c> unit tests in
/// <c>rust/smooth-operator/src/adapter.rs</c>; test names follow their Rust counterparts.
/// <para>
/// .NET has no conversation-name concept (its sidebar title is always the first inbound message, else
/// "Conversation"), so search runs over the first inbound message — the only title source this server
/// shows — and the default title never matches.
/// </para>
/// </summary>
public class ListConversationsPagingTests
{
    private const string Me = "me@smoo.ai";
    private const string Tara = "tara@smoo.ai";

    /// <summary>A settable clock, so the world can pin exact (and tied) <c>updatedAt</c> values.</summary>
    private sealed class ManualClock : TimeProvider
    {
        public DateTimeOffset Now { get; set; } = new(2026, 10, 8, 12, 0, 0, TimeSpan.Zero);

        public override DateTimeOffset GetUtcNow() => Now;
    }

    private sealed record World(
        InMemorySessionStore Store,
        ManualClock Clock,
        DateTimeOffset Base,
        List<string> Mine,
        string Acme,
        string Theirs,
        string Machine);

    private static AccessContext AuthedAs(string email) =>
        new(new Principal($"sub-{email}", "acme", "basic", Array.Empty<string>()) { Email = email }, IsAnonymous: false);

    private static AccessContext AuthedWithoutEmail() =>
        new(new Principal("sub-noemail", "acme", "basic", Array.Empty<string>()), IsAnonymous: false);

    private static async Task<string> SeedAsync(InMemorySessionStore store, ManualClock clock, DateTimeOffset at, string? email, string firstInbound)
    {
        clock.Now = at;
        var session = await store.CreateSessionAsync("agent", "N", email);
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, firstInbound);
        return session.ConversationId;
    }

    /// <summary>
    /// Seven of mine (three sharing one timestamp, so the id tiebreak straddles a page boundary at
    /// limit 3), one of another member's that mentions Acme, and one ownerless (legacy/widget) chat
    /// that mentions Acme. Mirrors the Rust <c>world()</c>.
    /// </summary>
    private static async Task<World> WorldAsync()
    {
        var clock = new ManualClock();
        var store = new InMemorySessionStore(clock);
        var t0 = clock.Now;
        DateTimeOffset Ago(int secs) => t0.AddSeconds(-secs).AddTicks(1234567); // sub-µs precision, kept exactly

        var tied = Ago(400);
        var times = new[] { Ago(10), Ago(20), tied, tied, tied, Ago(500), Ago(600) };
        var keyed = new List<(DateTimeOffset At, string Id)>();
        var acme = string.Empty;
        for (var i = 0; i < times.Length; i++)
        {
            var id = await SeedAsync(store, clock, times[i], Me, i == 5 ? "Where is the Acme invoice?" : "status update");
            if (i == 5)
            {
                acme = id;
            }
            keyed.Add((times[i], id));
        }
        var theirs = await SeedAsync(store, clock, Ago(15), Tara, "acme renewal");
        var machine = await SeedAsync(store, clock, Ago(25), null, "ACME widget chat");
        clock.Now = t0;

        var mine = keyed
            .OrderByDescending(k => k.At)
            .ThenByDescending(k => k.Id, StringComparer.Ordinal)
            .Select(k => k.Id)
            .ToList();
        return new World(store, clock, t0, mine, acme, theirs, machine);
    }

    private static FrameDispatcher Dispatcher(World w, AccessContext access) => new(w.Store, new MockChatClient(), access: access);

    private static async Task<JsonObject> ListAsync(FrameDispatcher dispatcher, JsonObject args)
    {
        args["action"] = "list_conversations";
        args["requestId"] = "r1";
        var events = new List<JsonObject>();
        await dispatcher.DispatchAsync(args.ToJsonString(), events.Add);
        return Assert.Single(events);
    }

    private static List<string> Ids(JsonObject ev) =>
        ev["data"]!["conversations"]!.AsArray().Select(c => c!["conversationId"]!.GetValue<string>()).ToList();

    private static async Task<List<List<string>>> AllPagesAsync(FrameDispatcher dispatcher, int limit, string? query = null)
    {
        var pages = new List<List<string>>();
        string? cursor = null;
        while (true)
        {
            var args = new JsonObject { ["limit"] = limit };
            if (cursor is not null)
            {
                args["cursor"] = cursor;
            }
            if (query is not null)
            {
                args["query"] = query;
            }
            var ev = await ListAsync(dispatcher, args);
            Assert.Equal("immediate_response", ev["type"]!.GetValue<string>());
            pages.Add(Ids(ev));
            var data = ev["data"]!.AsObject();
            var hasMore = data["hasMore"]!.GetValue<bool>();
            Assert.True(data.ContainsKey("nextCursor"), "nextCursor is always present (explicit null on the last page)");
            if (data["nextCursor"] is JsonNode next)
            {
                Assert.True(hasMore, "a nextCursor implies hasMore");
                cursor = next.GetValue<string>();
            }
            else
            {
                Assert.False(hasMore, "no nextCursor means no more");
                break;
            }
            Assert.True(pages.Count < 50, "paging never terminated");
        }
        return pages;
    }

    // ---- dispatcher: paging ------------------------------------------------------------------------

    [Fact]
    public async Task OwnedPages_AreDisjointAndComplete()
    {
        var w = await WorldAsync();
        var pages = await AllPagesAsync(Dispatcher(w, AuthedAs(Me)), 3);
        Assert.Equal(new[] { 3, 3, 1 }, pages.Select(p => p.Count));
        Assert.Equal(w.Mine, pages.SelectMany(p => p)); // pages concatenate to the full listing, in order
    }

    [Fact]
    public async Task UnscopedPages_AreDisjointAndComplete()
    {
        // The no-auth (single-tenant) scope sees every conversation, ownerless ones included.
        var w = await WorldAsync();
        var dispatcher = Dispatcher(w, AccessContext.Anonymous);
        var unpaged = Ids(await ListAsync(dispatcher, new JsonObject { ["limit"] = 100 }));
        Assert.Equal(9, unpaged.Count);
        var pages = await AllPagesAsync(dispatcher, 2);
        Assert.Equal(unpaged, pages.SelectMany(p => p));
        Assert.Contains(w.Machine, unpaged);
        Assert.Contains(w.Theirs, unpaged);
    }

    [Fact]
    public async Task APageTheSizeOfTheRest_ReportsNoMore()
    {
        var w = await WorldAsync();
        var ev = await ListAsync(Dispatcher(w, AuthedAs(Me)), new JsonObject { ["limit"] = 7 });
        Assert.Equal(w.Mine, Ids(ev));
        Assert.False(ev["data"]!["hasMore"]!.GetValue<bool>());
        Assert.True(ev["data"]!.AsObject().ContainsKey("nextCursor"));
        Assert.Null(ev["data"]!["nextCursor"]);
    }

    [Fact]
    public async Task NoCursor_IsTheOldFirstPage()
    {
        var w = await WorldAsync();
        var dispatcher = Dispatcher(w, AuthedAs(Me));
        // Default limit 50 returns every owned row.
        Assert.Equal(w.Mine, Ids(await ListAsync(dispatcher, new JsonObject())));
        // A blank cursor is no cursor.
        var ev = await ListAsync(dispatcher, new JsonObject { ["limit"] = 2, ["cursor"] = "" });
        Assert.Equal(w.Mine.Take(2), Ids(ev));
        Assert.True(ev["data"]!["hasMore"]!.GetValue<bool>());
    }

    [Fact]
    public async Task Response_MatchesTheListConversationsResponseSchema()
    {
        var w = await WorldAsync();
        var validator = await ProtocolValidator.LoadAsync();
        foreach (var limit in new[] { 3, 50 }) // a middle page (string cursor) and a last page (null cursor)
        {
            var ev = await ListAsync(Dispatcher(w, AuthedAs(Me)), new JsonObject { ["limit"] = limit });
            Assert.Equal("immediate_response", ev["type"]!.GetValue<string>());
            var result = validator.ValidateAt("actions/list-conversations.schema.json#/$defs/Response", ev["data"]!.ToJsonString());
            Assert.True(result.IsValid, result.FormatErrors());
        }
    }

    /// <summary>
    /// Bump one row on the NEXT page and one on the page already read, between pages. Neither may be
    /// returned twice; every untouched row must still come back; the bumped next-page row moves above
    /// the cursor and is not returned by the remaining pages (it heads a fresh first page instead).
    /// Mirrors the Rust <c>bump_between_pages</c>.
    /// </summary>
    [Fact]
    public async Task OwnedPaging_SurvivesConcurrentUpdates()
    {
        var w = await WorldAsync();
        var dispatcher = Dispatcher(w, AuthedAs(Me));

        var first = await ListAsync(dispatcher, new JsonObject { ["limit"] = 3 });
        var page1 = Ids(first);
        var cursor = first["data"]!["nextCursor"]!.GetValue<string>();

        var bumpedAhead = w.Mine[4]; // would have been on page 2
        var bumpedBehind = page1[1]; // already returned
        foreach (var id in new[] { bumpedAhead, bumpedBehind })
        {
            w.Clock.Now = w.Clock.Now.AddMilliseconds(2);
            await w.Store.AppendMessageAsync(id, MessageDirection.Outbound, "bump");
        }

        var rest = new List<string>();
        string? next = cursor;
        while (next is not null)
        {
            var ev = await ListAsync(dispatcher, new JsonObject { ["limit"] = 3, ["cursor"] = next });
            rest.AddRange(Ids(ev));
            next = ev["data"]!["nextCursor"]?.GetValue<string>();
        }

        var seen = page1.Concat(rest).ToList();
        Assert.Equal(seen.Count, seen.Distinct().Count()); // no row returned twice
        foreach (var id in w.Mine.Where(id => id != bumpedAhead))
        {
            Assert.Contains(id, seen); // untouched rows are never dropped
        }
        Assert.DoesNotContain(bumpedAhead, rest); // a row bumped above the cursor is not on later pages

        var fresh = await ListAsync(dispatcher, new JsonObject { ["limit"] = 1 });
        Assert.Equal(new[] { bumpedBehind }, Ids(fresh)); // the latest bump heads a fresh first page
    }

    // ---- dispatcher: search --------------------------------------------------------------------------

    [Fact]
    public async Task OwnedSearch_MatchesMyTitles_AndNeverWidensScope()
    {
        var w = await WorldAsync();
        // "acme" matches my first message, Tara's chat and the ownerless widget chat; only mine is listed.
        var ev = await ListAsync(Dispatcher(w, AuthedAs(Me)), new JsonObject { ["query"] = "  AcMe " });
        Assert.Equal(new[] { w.Acme }, Ids(ev));
        Assert.Equal("Where is the Acme invoice?", ev["data"]!["conversations"]![0]!["title"]!.GetValue<string>());
        Assert.False(ev["data"]!["hasMore"]!.GetValue<bool>());
        Assert.Null(ev["data"]!["nextCursor"]);
    }

    [Fact]
    public async Task Search_NeverReturnsAnotherMembersChat()
    {
        var w = await WorldAsync();
        var got = Ids(await ListAsync(Dispatcher(w, AuthedAs(Tara)), new JsonObject { ["query"] = "acme" }));
        Assert.Equal(new[] { w.Theirs }, got);
        Assert.DoesNotContain(w.Acme, got); // my matching chat never reaches Tara
        Assert.DoesNotContain(w.Machine, got); // .NET scoping lists no ownerless chat to a signed-in user
    }

    [Fact]
    public async Task UnscopedSearch_MatchesEveryConversationCaseInsensitively()
    {
        var w = await WorldAsync();
        var got = Ids(await ListAsync(Dispatcher(w, AccessContext.Anonymous), new JsonObject { ["query"] = "ACME" }));
        Assert.Equal(new[] { w.Acme, w.Machine, w.Theirs }.OrderBy(x => x), got.OrderBy(x => x));
    }

    [Fact]
    public async Task Search_DoesNotMatchTheDefaultTitle()
    {
        // .NET's analog of `search_matches_a_meaningful_name_but_not_the_default_placeholder`: a
        // conversation with no inbound message is titled with the default "Conversation"; that
        // placeholder is not a title source, so searching for it matches nothing.
        var clock = new ManualClock();
        var store = new InMemorySessionStore(clock);
        var session = await store.CreateSessionAsync("agent", "N", Me);
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Outbound, "Hi! How can I help?");
        var dispatcher = new FrameDispatcher(store, new MockChatClient(), access: AuthedAs(Me));

        var all = await ListAsync(dispatcher, new JsonObject());
        Assert.Equal("Conversation", all["data"]!["conversations"]![0]!["title"]!.GetValue<string>());
        Assert.Empty(Ids(await ListAsync(dispatcher, new JsonObject { ["query"] = "conversation" })));
        Assert.Empty(Ids(await ListAsync(dispatcher, new JsonObject { ["query"] = "help" }))); // outbound text is not a title source
    }

    [Fact]
    public async Task BlankQuery_IsNoFilter()
    {
        var w = await WorldAsync();
        Assert.Equal(w.Mine, Ids(await ListAsync(Dispatcher(w, AuthedAs(Me)), new JsonObject { ["query"] = "   " })));
    }

    [Fact]
    public async Task SearchPages_WithACursor()
    {
        var w = await WorldAsync();
        var pages = await AllPagesAsync(Dispatcher(w, AuthedAs(Me)), 2, "status");
        Assert.Equal(new[] { 2, 2, 2 }, pages.Select(p => p.Count));
        // The six "status update" rows, in order, once each.
        Assert.Equal(w.Mine.Where(id => id != w.Acme), pages.SelectMany(p => p));
    }

    // ---- dispatcher: validation + fail-closed scope --------------------------------------------------

    [Theory]
    [InlineData("not-a-cursor")]
    [InlineData("bm9waXBl")] // "nopipe"
    [InlineData("MjAyNi0xMC0wOFQwMDowMDowMFp8")] // "2026-10-08T00:00:00Z|" — empty id
    [InlineData("   ")]
    public async Task AnUnknownCursor_IsAValidationError(string cursor)
    {
        var w = await WorldAsync();
        var ev = await ListAsync(Dispatcher(w, AuthedAs(Me)), new JsonObject { ["cursor"] = cursor });
        Assert.Equal("error", ev["type"]!.GetValue<string>());
        Assert.Equal("VALIDATION_ERROR", ev["data"]!["error"]!["code"]!.GetValue<string>());
    }

    [Fact]
    public async Task AnEmaillessPrincipal_PagesNothing()
    {
        var w = await WorldAsync();
        var ev = await ListAsync(Dispatcher(w, AuthedWithoutEmail()), new JsonObject { ["query"] = "acme" });
        Assert.Empty(Ids(ev));
        Assert.False(ev["data"]!["hasMore"]!.GetValue<bool>());
    }

    // ---- ConversationKey / ConversationPageQuery (adapter.rs unit tests) -----------------------------

    [Fact]
    public void Cursor_RoundTripsAtFullPrecision()
    {
        var precise = new ConversationKey(new DateTimeOffset(2026, 10, 8, 14, 30, 0, TimeSpan.Zero).AddTicks(1234567), "3f2a0c4e-0000-4000-8000-000000000001");
        Assert.True(ConversationKey.TryDecode(precise.Encode(), out var back));
        Assert.Equal(precise, back);
        Assert.Equal(precise.UpdatedAt.UtcTicks, back!.UpdatedAt.UtcTicks);

        var whole = new ConversationKey(new DateTimeOffset(2026, 10, 8, 14, 30, 0, TimeSpan.Zero), "a");
        Assert.True(ConversationKey.TryDecode(whole.Encode(), out var wholeBack));
        Assert.Equal(whole, wholeBack);
    }

    [Fact]
    public void Cursor_EncodesAsUnpaddedBase64UrlOfRfc3339PipeId()
    {
        var key = new ConversationKey(new DateTimeOffset(2026, 10, 8, 14, 30, 0, TimeSpan.Zero).AddTicks(1234560), "33333333-3333-3333-3333-333333333333");
        var encoded = key.Encode();
        Assert.DoesNotContain('=', encoded);
        Assert.DoesNotContain('+', encoded);
        Assert.DoesNotContain('/', encoded);
        // The exact cursor the Rust server emits for this key (spec/conformance fixtures).
        Assert.Equal("MjAyNi0xMC0wOFQxNDozMDowMC4xMjM0NTZafDMzMzMzMzMzLTMzMzMtMzMzMy0zMzMzLTMzMzMzMzMzMzMzMw", encoded);
    }

    [Theory]
    [InlineData("2026-10-08T14:30:00.123456789Z", 1234567)] // nanoseconds from Rust truncate to ticks
    [InlineData("2026-10-08T14:30:00.123456Z", 1234560)] // Postgres µs
    [InlineData("2026-10-08T16:30:00.5+02:00", 5000000)] // an offset normalises to UTC
    [InlineData("2026-10-08t14:30:00z", 0)]
    public void Cursor_DecodesAnyRfc3339Timestamp(string ts, long fractionTicks)
    {
        var cursor = Base64Url($"{ts}|id-1");
        Assert.True(ConversationKey.TryDecode(cursor, out var key));
        Assert.Equal(new DateTimeOffset(2026, 10, 8, 14, 30, 0, TimeSpan.Zero).AddTicks(fractionTicks), key!.UpdatedAt);
        Assert.Equal(TimeSpan.Zero, key.UpdatedAt.Offset);
        Assert.Equal("id-1", key.Id);
    }

    [Theory]
    [InlineData("")]
    [InlineData("not base64!")]
    [InlineData("bm9waXBl")]
    [InlineData("MjAyNi0xMC0wOFQwMDowMDowMFp8")]
    [InlineData("MjAyNi0xMC0wOFQwMDowMDowMFp8YQ==")] // padded: not unpadded base64url
    public void ForeignCursors_DoNotDecode(string bad)
    {
        Assert.False(ConversationKey.TryDecode(bad, out _));
    }

    [Theory]
    [InlineData("yesterday|id")]
    [InlineData("2026-10-08 14:30:00|id")] // no timezone
    [InlineData("2026-13-08T14:30:00Z|id")]
    public void Cursors_WithABadTimestamp_DoNotDecode(string raw)
    {
        Assert.False(ConversationKey.TryDecode(Base64Url(raw), out _));
    }

    [Fact]
    public void Precedes_IsNewestFirstThenIdDescending()
    {
        var at = (int s) => DateTimeOffset.FromUnixTimeSeconds(s);
        var k = new ConversationKey(at(100), "m");
        Assert.True(k.Precedes(at(99), "z")); // older sorts after
        Assert.False(k.Precedes(at(101), "a")); // newer sorts before
        Assert.True(k.Precedes(at(100), "a")); // same time, smaller id sorts after
        Assert.False(k.Precedes(at(100), "m")); // the key itself is not after itself
        Assert.False(k.Precedes(at(100), "z")); // same time, larger id sorts before
        Assert.True(k.Precedes(at(100), "M")); // ordinal, not culture: 'M' < 'm'
        Assert.True(k.Precedes(new DateTimeOffset(at(100).UtcDateTime.AddTicks(-1), TimeSpan.Zero), "z")); // full tick precision
    }

    [Fact]
    public void Search_MatchesFirstMessageCaseInsensitively()
    {
        var q = ConversationPageQuery.Create(10, after: null, search: "  ACME ");
        Assert.Equal("ACME", q.Search);
        Assert.True(q.MatchesSearch("where is the acme invoice"));
        Assert.False(q.MatchesSearch(null));
        Assert.False(q.MatchesSearch("nothing here"));
        var blank = ConversationPageQuery.Create(10, after: null, search: "   ");
        Assert.Null(blank.Search);
        Assert.True(blank.MatchesSearch(null));
    }

    private static string Base64Url(string raw) =>
        Convert.ToBase64String(Encoding.UTF8.GetBytes(raw)).TrimEnd('=').Replace('+', '-').Replace('/', '_');

    /// <summary>
    /// A pre-paging client asking for 1000 gets the 200-row maximum and a cursor for the rest, not
    /// an error. Rust: <c>a_limit_over_the_maximum_is_clamped_not_rejected</c>.
    /// </summary>
    [Fact]
    public async Task ALimitOverTheMaximum_IsClampedNotRejected()
    {
        var clock = new ManualClock();
        var store = new InMemorySessionStore(clock);
        var t0 = clock.Now;
        for (var i = 0; i < 205; i++)
        {
            await SeedAsync(store, clock, t0.AddSeconds(-i), Me, "status update");
        }
        clock.Now = t0;
        var dispatcher = new FrameDispatcher(store, new MockChatClient(), access: AuthedAs(Me));

        var first = await ListAsync(dispatcher, new JsonObject { ["limit"] = 1000 });
        Assert.Equal("immediate_response", first["type"]!.GetValue<string>());
        Assert.Equal(200, Ids(first).Count);
        Assert.True(first["data"]!["hasMore"]!.GetValue<bool>());

        var rest = await ListAsync(dispatcher, new JsonObject { ["limit"] = 1000, ["cursor"] = first["data"]!["nextCursor"]!.GetValue<string>() });
        Assert.Equal(5, Ids(rest).Count);
        Assert.False(rest["data"]!["hasMore"]!.GetValue<bool>());
    }
}
