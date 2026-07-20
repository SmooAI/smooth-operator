using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// SECURITY (th-966fab, epic th-8fe998): per-user scoping of the conversation surface.
/// <para>
/// Before this fix <c>list_conversations</c> returned EVERY user's conversations, and <c>resume</c> /
/// <c>get_conversation_messages</c> / <c>get_session</c> were not owner-checked — any authenticated
/// user could enumerate and open anyone else's chats. These tests are written from the ATTACKER's
/// side: user B is the victim, user A is the attacker, and every assertion is about what A can learn.
/// </para>
/// </summary>
public class UserScopingSecurityTests
{
    private const string AttackerEmail = "attacker@example.com";
    private const string VictimEmail = "victim@example.com";

    /// <summary>A connection authenticated as <paramref name="email"/> (auth ENABLED).</summary>
    private static AccessContext AuthedAs(string email) =>
        new(new Principal($"sub-{email}", "acme", "basic", Array.Empty<string>()) { Email = email }, IsAnonymous: false);

    /// <summary>Auth ENABLED but the principal carries no email claim — must fail closed.</summary>
    private static AccessContext AuthedWithoutEmail() =>
        new(new Principal("sub-noemail", "acme", "basic", Array.Empty<string>()), IsAnonymous: false);

    private static FrameDispatcher Dispatcher(ISessionStore store, AccessContext? access = null) =>
        new(store, new MockChatClient(), access: access);

    /// <summary>Create a conversation owned by <paramref name="email"/>, with one message so it lists.</summary>
    private static async Task<StoredSession> SeedConversationAsync(InMemorySessionStore store, string email, string firstLine)
    {
        var session = await store.CreateSessionAsync("agent", "N", email);
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, firstLine);
        return session;
    }

    /// <summary>
    /// An error payload with the wall-clock <c>timestamp</c> dropped and the caller-supplied id masked
    /// — everything else must be byte-identical between "not yours" and "never existed", or the
    /// difference is an oracle.
    /// </summary>
    private static string Normalize(JsonObject ev, string suppliedId)
    {
        var copy = ev.DeepClone().AsObject();
        copy.Remove("timestamp");
        return copy.ToJsonString().Replace(suppliedId, "ID", StringComparison.Ordinal);
    }

    private static JsonObject Dispatch(FrameDispatcher dispatcher, string frame)
    {
        var events = new List<JsonObject>();
        dispatcher.DispatchAsync(frame, events.Add).GetAwaiter().GetResult();
        return Assert.Single(events);
    }

    // ---- list_conversations is scoped to the authenticated principal ------------------------------

    [Fact]
    public async Task ListConversations_ReturnsOnlyTheCallersOwnConversations()
    {
        var store = new InMemorySessionStore();
        var mine = await SeedConversationAsync(store, AttackerEmail, "my own question");
        var theirs = await SeedConversationAsync(store, VictimEmail, "victim's private question");

        var ev = Dispatch(Dispatcher(store, AuthedAs(AttackerEmail)), """{"action":"list_conversations","requestId":"r1"}""");

        var conversations = ev["data"]!["conversations"]!.AsArray();
        var only = Assert.Single(conversations);
        Assert.Equal(mine.ConversationId, only!["conversationId"]!.GetValue<string>());
        // The victim's id AND their message text must be nowhere in the payload.
        var json = ev.ToJsonString();
        Assert.DoesNotContain(theirs.ConversationId, json, StringComparison.Ordinal);
        Assert.DoesNotContain("victim's private question", json, StringComparison.Ordinal);
    }

    [Fact]
    public async Task ListConversations_AuthEnabledPrincipalWithoutEmail_IsEmptyNotUnscoped()
    {
        var store = new InMemorySessionStore();
        await SeedConversationAsync(store, VictimEmail, "victim's private question");
        await SeedConversationAsync(store, AttackerEmail, "someone else's question");

        var ev = Dispatch(Dispatcher(store, AuthedWithoutEmail()), """{"action":"list_conversations","requestId":"r1"}""");

        Assert.Empty(ev["data"]!["conversations"]!.AsArray());
    }

    [Fact]
    public async Task ListConversations_AuthEnabledBadToken_IsEmptyNotUnscoped()
    {
        // A jwt-mode server handed a garbage token resolves to anonymous — but auth is still ENABLED,
        // so it must fail closed rather than inherit the no-auth unscoped behavior.
        var access = new TokenAccessResolver(new AuthOptions { Mode = AuthMode.Jwt, Hs256Secret = "s3cret" }).Resolve("not.a.jwt");
        Assert.True(access.AuthEnabled);

        var store = new InMemorySessionStore();
        await SeedConversationAsync(store, VictimEmail, "victim's private question");

        var ev = Dispatch(Dispatcher(store, access), """{"action":"list_conversations","requestId":"r1"}""");

        Assert.Empty(ev["data"]!["conversations"]!.AsArray());
    }

    [Fact]
    public async Task ListConversations_AuthDisabled_StaysUnscoped()
    {
        // The single-tenant local/dev path: no auth configured, no notion of a user → unchanged.
        var store = new InMemorySessionStore();
        await SeedConversationAsync(store, VictimEmail, "one");
        await SeedConversationAsync(store, AttackerEmail, "two");

        var ev = Dispatch(Dispatcher(store, AccessContext.Anonymous), """{"action":"list_conversations","requestId":"r1"}""");

        Assert.Equal(2, ev["data"]!["conversations"]!.AsArray().Count);
    }

    // ---- resume is owner-checked, and indistinguishable from not-found ----------------------------

    [Fact]
    public async Task Resume_OfAnotherUsersConversation_IsIdenticalToAnIdThatNeverExisted()
    {
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");
        var attacker = Dispatcher(store, AuthedAs(AttackerEmail));

        var stolen = Dispatch(attacker, $$"""{"action":"create_conversation_session","requestId":"r1","agentId":"a","conversationId":"{{victim.ConversationId}}"}""");
        var phantom = Dispatch(attacker, """{"action":"create_conversation_session","requestId":"r1","agentId":"a","conversationId":"does-not-exist"}""");

        Assert.Equal("SESSION_NOT_FOUND", stolen["error"]!["code"]!.GetValue<string>());

        // THE EXISTENCE-ORACLE TEST: the two payloads must differ ONLY in the id the attacker supplied.
        // Anything else — a different code, a different message, a 200 that mints a session for the
        // unknown id — tells the attacker which conversation ids are real.
        Assert.Equal(Normalize(phantom, "does-not-exist"), Normalize(stolen, victim.ConversationId));
    }

    [Fact]
    public async Task Resume_OfOwnConversation_StillWorks()
    {
        var store = new InMemorySessionStore();
        var mine = await SeedConversationAsync(store, AttackerEmail, "my own question");

        var ev = Dispatch(
            Dispatcher(store, AuthedAs(AttackerEmail)),
            $$"""{"action":"create_conversation_session","requestId":"r1","agentId":"a","conversationId":"{{mine.ConversationId}}"}""");

        Assert.Equal(200, ev["status"]!.GetValue<int>());
        Assert.Equal(mine.ConversationId, ev["data"]!["conversationId"]!.GetValue<string>());
    }

    // ---- the principal wins over client-supplied identity ----------------------------------------

    [Fact]
    public async Task CreateSession_IgnoresClientSuppliedUserEmail_PrincipalWins()
    {
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");

        // The attacker claims to be the victim in the frame. The connection says otherwise.
        var attacker = Dispatcher(store, AuthedAs(AttackerEmail));
        var created = Dispatch(
            attacker,
            $$"""{"action":"create_conversation_session","requestId":"r1","agentId":"a","userName":"Victim","userEmail":"{{VictimEmail}}"}""");
        Assert.Equal(200, created["status"]!.GetValue<int>());

        // The spoofed email must not have been stamped on the session…
        var sessionId = created["data"]!["sessionId"]!.GetValue<string>();
        Assert.Equal(AttackerEmail, (await store.GetSessionAsync(sessionId))!.UserEmail);

        // …and must not have bought the attacker the victim's scope.
        var listed = Dispatch(attacker, """{"action":"list_conversations","requestId":"r2"}""");
        Assert.DoesNotContain(victim.ConversationId, listed.ToJsonString(), StringComparison.Ordinal);
    }

    // ---- get_conversation_messages / get_session are owner-checked --------------------------------

    [Fact]
    public async Task GetConversationMessages_OfAnotherUsersSession_IsIdenticalToAnIdThatNeverExisted()
    {
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");
        var attacker = Dispatcher(store, AuthedAs(AttackerEmail));

        var stolen = Dispatch(attacker, $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{victim.SessionId}}"}""");
        var phantom = Dispatch(attacker, """{"action":"get_conversation_messages","requestId":"r1","sessionId":"does-not-exist"}""");

        Assert.Equal("SESSION_NOT_FOUND", stolen["error"]!["code"]!.GetValue<string>());
        Assert.DoesNotContain("victim's private question", stolen.ToJsonString(), StringComparison.Ordinal);

        Assert.Equal(Normalize(phantom, "does-not-exist"), Normalize(stolen, victim.SessionId));
    }

    [Fact]
    public async Task GetSession_OfAnotherUsersSession_IsIdenticalToAnIdThatNeverExisted()
    {
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");
        var attacker = Dispatcher(store, AuthedAs(AttackerEmail));

        var stolen = Dispatch(attacker, $$"""{"action":"get_session","requestId":"r1","sessionId":"{{victim.SessionId}}"}""");
        var phantom = Dispatch(attacker, """{"action":"get_session","requestId":"r1","sessionId":"does-not-exist"}""");

        Assert.Equal("SESSION_NOT_FOUND", stolen["error"]!["code"]!.GetValue<string>());
        // get_session echoes conversationId on success — it must not leak the victim's.
        Assert.DoesNotContain(victim.ConversationId, stolen.ToJsonString(), StringComparison.Ordinal);

        Assert.Equal(Normalize(phantom, "does-not-exist"), Normalize(stolen, victim.SessionId));
    }

    [Fact]
    public async Task GetConversationMessages_AuthEnabledPrincipalWithoutEmail_IsDenied()
    {
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");

        var ev = Dispatch(
            Dispatcher(store, AuthedWithoutEmail()),
            $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{victim.SessionId}}"}""");

        Assert.Equal("SESSION_NOT_FOUND", ev["error"]!["code"]!.GetValue<string>());
    }

    [Fact]
    public async Task GetConversationMessages_OfOwnSession_StillWorks()
    {
        var store = new InMemorySessionStore();
        var mine = await SeedConversationAsync(store, AttackerEmail, "my own question");

        var ev = Dispatch(
            Dispatcher(store, AuthedAs(AttackerEmail)),
            $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{mine.SessionId}}"}""");

        Assert.Equal(200, ev["status"]!.GetValue<int>());
        Assert.Single(ev["data"]!["messages"]!.AsArray());
    }

    // ---- legacy data (no recorded owner) fails closed under auth ----------------------------------

    [Fact]
    public async Task ConversationWithNoRecordedOwner_IsInvisibleToEveryAuthenticatedUser()
    {
        // Rows written before per-user scoping existed carry a NULL user_email. They belong to nobody
        // and must not fall to the first caller who asks.
        var store = new InMemorySessionStore();
        var legacy = await SeedConversationAsync(store, email: null!, firstLine: "pre-scoping conversation");
        Assert.Null(legacy.UserEmail);

        var attacker = Dispatcher(store, AuthedAs(AttackerEmail));
        Assert.Empty(Dispatch(attacker, """{"action":"list_conversations","requestId":"r1"}""")["data"]!["conversations"]!.AsArray());
        Assert.Equal(
            "SESSION_NOT_FOUND",
            Dispatch(attacker, $$"""{"action":"get_conversation_messages","requestId":"r2","sessionId":"{{legacy.SessionId}}"}""")["error"]!["code"]!.GetValue<string>());
        Assert.False(await store.ConversationBelongsToUserAsync(legacy.ConversationId, AttackerEmail));
    }

    // ---- the scope type + auth plumbing ----------------------------------------------------------

    [Fact]
    public void TokenAccessResolver_LiftsTheEmailClaimOntoThePrincipal()
    {
        var payload = System.Text.Json.JsonSerializer.Serialize(new { sub = "u1", org = "acme", email = VictimEmail });
        var token = Convert.ToBase64String(System.Text.Encoding.UTF8.GetBytes(payload)).TrimEnd('=').Replace('+', '-').Replace('/', '_');

        var access = new TokenAccessResolver(new AuthOptions { Mode = AuthMode.Trusted }).Resolve(token);

        Assert.Equal(VictimEmail, access.Principal.Email);
        Assert.True(access.AuthEnabled);
        Assert.Equal(VictimEmail, access.ConversationScope.UserEmail);
        Assert.False(access.ConversationScope.IsUnscoped);
    }

    [Fact]
    public void ConversationScope_UnscopedIsOnlyReachableWithAuthOff()
    {
        Assert.True(AccessContext.Anonymous.ConversationScope.IsUnscoped);
        Assert.False(AccessContext.Anonymous.AuthEnabled);

        // Auth on, no email → None (empty), NEVER unscoped.
        var noEmail = AuthedWithoutEmail().ConversationScope;
        Assert.False(noEmail.IsUnscoped);
        Assert.True(noEmail.IsEmpty);

        // Auth on with an email → scoped to it.
        var scoped = AuthedAs(AttackerEmail).ConversationScope;
        Assert.False(scoped.IsUnscoped);
        Assert.False(scoped.IsEmpty);
        Assert.Equal(AttackerEmail, scoped.UserEmail);
    }

    [Fact]
    public async Task ConversationBelongsToUser_IsFalseForOtherUsersAndForUnknownIds()
    {
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");

        Assert.False(await store.ConversationBelongsToUserAsync(victim.ConversationId, AttackerEmail));
        Assert.False(await store.ConversationBelongsToUserAsync("never-existed", AttackerEmail));
        Assert.True(await store.ConversationBelongsToUserAsync(victim.ConversationId, VictimEmail));
        // Email comparison is case-insensitive — the same human, not a second account.
        Assert.True(await store.ConversationBelongsToUserAsync(victim.ConversationId, VictimEmail.ToUpperInvariant()));
    }
}
