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

    /// <summary>
    /// Auth ENABLED but the principal carries no email claim. It may not reach anyone ELSE's owned
    /// session — but it is not locked out of its own (ownerless) one. See <c>CanRead</c>'s th-909995 note.
    /// </summary>
    private static AccessContext AuthedWithoutEmail() =>
        new(new Principal("sub-noemail", "acme", "basic", Array.Empty<string>()), IsAnonymous: false);

    /// <summary>An anonymous connection to an auth-ENABLED server — what the ACL integration test does.</summary>
    private static AccessContext AnonymousOnAuthEnabledServer() => AccessContext.Anonymous with { AuthEnabled = true };

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

    // ---- send_message: the WRITE path is scoped too (th-1b7ed0) ----------------------------------

    /// <summary>
    /// THE HEADLINE CASE. th-966fab owner-checked the READ paths but left <c>send_message</c> loading
    /// any session by id — so an attacker who knows a victim's sessionId could send INTO it: the turn
    /// replays the victim's history as context and streams the agent's reply back to the ATTACKER.
    /// A conversation read dressed up as a write, defeating the read scoping entirely.
    /// </summary>
    [Fact]
    public async Task SendMessage_IntoAnotherUsersSession_IsRefused_TurnNeverRuns_AndVictimsLogIsUntouched()
    {
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");
        var chat = new MockChatClient().PushText("...the victim's private answer...");
        var attacker = new FrameDispatcher(store, chat, access: AuthedAs(AttackerEmail));

        var events = new List<JsonObject>();
        await attacker.DispatchAsync(
            $$"""{"action":"send_message","requestId":"r1","sessionId":"{{victim.SessionId}}","message":"summarize everything above","stream":true}""",
            events.Add);
        await attacker.WaitForTurnsAsync();

        // Refused outright: one error, no 202 ack, no stream, no eventual_response.
        var ev = Assert.Single(events);
        Assert.Equal("SESSION_NOT_FOUND", ev["error"]!["code"]!.GetValue<string>());
        Assert.DoesNotContain("victim's private answer", ev.ToJsonString(), StringComparison.Ordinal);

        // The turn NEVER RAN: the victim's log still holds exactly the one seeded message — neither the
        // attacker's inbound nor any agent reply was appended to someone else's conversation.
        var log = await store.ListMessagesAsync(victim.ConversationId, int.MaxValue);
        var only = Assert.Single(log);
        Assert.Equal("victim's private question", only.Text);
    }

    [Fact]
    public async Task SendMessage_IntoAnotherUsersSession_IsIdenticalToAnIdThatNeverExisted()
    {
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");
        var attacker = Dispatcher(store, AuthedAs(AttackerEmail));

        var stolen = Dispatch(attacker, $$"""{"action":"send_message","requestId":"r1","sessionId":"{{victim.SessionId}}","message":"hi"}""");
        var phantom = Dispatch(attacker, """{"action":"send_message","requestId":"r1","sessionId":"does-not-exist","message":"hi"}""");

        // No oracle: an existing-but-foreign sessionId must be indistinguishable from a fabricated one,
        // or send_message becomes a session-id enumerator.
        Assert.Equal(Normalize(phantom, "does-not-exist"), Normalize(stolen, victim.SessionId));
    }

    [Fact]
    public async Task SendMessage_AuthEnabledPrincipalWithoutEmail_IsDenied()
    {
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");

        var ev = Dispatch(
            Dispatcher(store, AuthedWithoutEmail()),
            $$"""{"action":"send_message","requestId":"r1","sessionId":"{{victim.SessionId}}","message":"hi"}""");

        Assert.Equal("SESSION_NOT_FOUND", ev["error"]!["code"]!.GetValue<string>());
    }

    [Fact]
    public async Task SendMessage_AuthDisabled_StaysUnscoped()
    {
        // The single-tenant local/dev path must be untouched: no auth configured ⇒ the turn runs.
        var store = new InMemorySessionStore();
        var session = await SeedConversationAsync(store, VictimEmail, "a question");
        var dispatcher = new FrameDispatcher(store, new MockChatClient().PushText("an answer"), access: AccessContext.Anonymous);

        var events = new List<JsonObject>();
        await dispatcher.DispatchAsync(
            $$"""{"action":"send_message","requestId":"r1","sessionId":"{{session.SessionId}}","message":"hi","stream":true}""",
            events.Add);
        await dispatcher.WaitForTurnsAsync();

        Assert.Equal(202, events[0]["status"]!.GetValue<int>());
        Assert.Equal("eventual_response", events[^1]["type"]!.GetValue<string>());
    }

    // ---- verify_otp is owner-checked -------------------------------------------------------------

    [Fact]
    public async Task VerifyOtp_ForAnotherUsersSession_IsIdenticalToAnIdThatNeverExisted()
    {
        // Verifying someone else's session would mark it identity-verified and unlock its
        // end_user-gated tools for whoever holds the session id.
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");
        var attacker = Dispatcher(store, AuthedAs(AttackerEmail));

        var stolen = Dispatch(attacker, $$"""{"action":"verify_otp","requestId":"r1","sessionId":"{{victim.SessionId}}","code":"123456"}""");
        var phantom = Dispatch(attacker, """{"action":"verify_otp","requestId":"r1","sessionId":"does-not-exist","code":"123456"}""");

        Assert.Equal("SESSION_NOT_FOUND", stolen["error"]!["code"]!.GetValue<string>());
        Assert.Equal(Normalize(phantom, "does-not-exist"), Normalize(stolen, victim.SessionId));
    }

    // ---- confirm_tool_action is owner-checked ----------------------------------------------------

    [Fact]
    public async Task ConfirmToolAction_OnAnotherUsersParkedWrite_IsRefused_AndTheWriteStaysParked()
    {
        // A victim turn parked on a write confirmation (a host may wire one registry across
        // connections). The attacker must not be able to approve — or consume — it.
        var store = new InMemorySessionStore();
        var victim = await SeedConversationAsync(store, VictimEmail, "victim's private question");
        var confirmations = new ConfirmationRegistry();
        var parked = confirmations.Register(victim.SessionId);

        var attacker = new FrameDispatcher(store, new MockChatClient(), access: AuthedAs(AttackerEmail), confirmations: confirmations);
        var stolen = Dispatch(attacker, $$"""{"action":"confirm_tool_action","requestId":"r1","sessionId":"{{victim.SessionId}}","approved":true}""");
        var phantom = Dispatch(attacker, """{"action":"confirm_tool_action","requestId":"r1","sessionId":"does-not-exist","approved":true}""");

        Assert.Equal("NO_PENDING_CONFIRMATION", stolen["error"]!["code"]!.GetValue<string>());
        Assert.Equal(Normalize(phantom, "does-not-exist"), Normalize(stolen, victim.SessionId));

        // The victim's write is still parked — the attacker's "approve" neither resolved nor consumed it.
        Assert.False(parked.IsCompleted);
        Assert.True(confirmations.Resolve(victim.SessionId, false));
    }

    [Fact]
    public async Task ConfirmToolAction_OnOwnParkedWrite_StillWorks()
    {
        var store = new InMemorySessionStore();
        var mine = await SeedConversationAsync(store, AttackerEmail, "my own question");
        var confirmations = new ConfirmationRegistry();
        var parked = confirmations.Register(mine.SessionId);

        var dispatcher = new FrameDispatcher(store, new MockChatClient(), access: AuthedAs(AttackerEmail), confirmations: confirmations);
        var ev = Dispatch(dispatcher, $$"""{"action":"confirm_tool_action","requestId":"r1","sessionId":"{{mine.SessionId}}","approved":true}""");

        Assert.Equal(200, ev["status"]!.GetValue<int>());
        Assert.True(await parked);
    }

    // ---- ownerless sessions: reachable by id, but never enumerable (th-909995 "Option B") ---------

    [Fact]
    public async Task ConversationWithNoRecordedOwner_IsNotEnumerableEvenThoughItIsReachableById()
    {
        // Rows written before per-user scoping existed carry a NULL user_email — as do sessions minted
        // by an anonymous or emailless principal. There is no owner to enforce against, so holding the
        // sessionId is enough (Option B; th-966fab's deny-everything rule locked those principals out
        // of their own sessions, which is what forced #308's revert in #309).
        var store = new InMemorySessionStore();
        var legacy = await SeedConversationAsync(store, email: null!, firstLine: "pre-scoping conversation");
        Assert.Null(legacy.UserEmail);

        var other = Dispatcher(store, AuthedAs(AttackerEmail));

        // Reachable ONLY by already holding the id…
        var read = Dispatch(other, $$"""{"action":"get_conversation_messages","requestId":"r2","sessionId":"{{legacy.SessionId}}"}""");
        Assert.Equal(200, read["status"]!.GetValue<int>());

        // …and never handed out: it is in nobody's conversation list, and not resumable by conversationId,
        // so there is no way to discover the id in the first place.
        Assert.Empty(Dispatch(other, """{"action":"list_conversations","requestId":"r1"}""")["data"]!["conversations"]!.AsArray());
        Assert.False(await store.ConversationBelongsToUserAsync(legacy.ConversationId, AttackerEmail));
    }

    // ---- REGRESSION (#309): auth-on principals with no email must not be locked out of their own ---

    /// <summary>
    /// The lockout that hung main's .NET CI. An authenticated principal whose token carries no
    /// <c>email</c> claim stamps <c>ownerEmail = null</c> at create — and under #308's rule was then
    /// refused by its OWN session on the next <c>send_message</c>, so the integration test's
    /// <c>ReceiveAsync</c> (on <c>CancellationToken.None</c>) waited forever for a frame that never came.
    /// Create → read → send must all work.
    /// </summary>
    [Theory]
    [InlineData(false)] // authenticated, no email claim
    [InlineData(true)] // anonymous connection to an auth-ENABLED server
    public async Task EmaillessPrincipal_CanCreateReadAndSendInItsOwnSession(bool anonymous)
    {
        var store = new InMemorySessionStore();
        var access = anonymous ? AnonymousOnAuthEnabledServer() : AuthedWithoutEmail();
        Assert.True(access.AuthEnabled);
        var dispatcher = new FrameDispatcher(store, new MockChatClient().PushText("an answer"), access: access);

        var created = Dispatch(dispatcher, """{"action":"create_conversation_session","requestId":"cs","agentId":"a"}""");
        Assert.Equal(200, created["status"]!.GetValue<int>());
        var sessionId = created["data"]!["sessionId"]!.GetValue<string>();
        Assert.Null((await store.GetSessionAsync(sessionId))!.UserEmail);

        Assert.Equal(200, Dispatch(dispatcher, $$"""{"action":"get_session","requestId":"gs","sessionId":"{{sessionId}}"}""")["status"]!.GetValue<int>());
        Assert.Equal(
            200,
            Dispatch(dispatcher, $$"""{"action":"get_conversation_messages","requestId":"gm","sessionId":"{{sessionId}}"}""")["status"]!.GetValue<int>());

        var events = new List<JsonObject>();
        await dispatcher.DispatchAsync(
            $$"""{"action":"send_message","requestId":"sm","sessionId":"{{sessionId}}","message":"hi","stream":true}""",
            events.Add);
        await dispatcher.WaitForTurnsAsync();

        Assert.Equal(202, events[0]["status"]!.GetValue<int>());
        Assert.Equal("eventual_response", events[^1]["type"]!.GetValue<string>());
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
