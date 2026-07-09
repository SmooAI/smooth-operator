using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Postgres.Tests;

/// <summary>
/// The <see cref="ISessionStore"/> behavioral contract — run against BOTH the in-memory and the
/// Postgres adapter, so they're provably interchangeable. This is the C# version of the Rust
/// pattern where one adapter contract is asserted against every backend.
/// </summary>
public abstract class SessionStoreContractTests
{
    /// <summary>Provide a fresh store. May Skip (e.g. Postgres when Docker is unavailable).</summary>
    protected abstract Task<ISessionStore> CreateStoreAsync();

    [SkippableFact]
    public async Task CreateSession_ThenGet_RoundTrips()
    {
        var store = await CreateStoreAsync();

        var created = await store.CreateSessionAsync(agentId: "", userName: "Alice", userEmail: null);
        Assert.True(Guid.TryParse(created.SessionId, out _));
        Assert.True(Guid.TryParse(created.ConversationId, out _));

        var fetched = await store.GetSessionAsync(created.SessionId);
        Assert.NotNull(fetched);
        Assert.Equal(created.ConversationId, fetched!.ConversationId);
        Assert.Equal(created.AgentId, fetched.AgentId);
        Assert.Equal(created.AgentParticipantId, fetched.AgentParticipantId);

        Assert.Null(await store.GetSessionAsync("does-not-exist"));
    }

    [SkippableFact]
    public async Task AppendAndList_PreservesOrderAndDirection()
    {
        var store = await CreateStoreAsync();
        var session = await store.CreateSessionAsync("", null, null);

        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "hello");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Outbound, "hi there");

        var messages = await store.ListMessagesAsync(session.ConversationId, 50);
        Assert.Equal(2, messages.Count);
        Assert.Equal("hello", messages[0].Text);
        Assert.Equal(MessageDirection.Inbound, messages[0].Direction);
        Assert.Equal("hi there", messages[1].Text);
        Assert.Equal(MessageDirection.Outbound, messages[1].Direction);
    }

    [SkippableFact]
    public async Task List_RespectsLimit_ReturnsMostRecentOldestFirst()
    {
        var store = await CreateStoreAsync();
        var session = await store.CreateSessionAsync("", null, null);
        for (var i = 0; i < 5; i++)
        {
            await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, $"m{i}");
        }

        var messages = await store.ListMessagesAsync(session.ConversationId, 2);
        Assert.Equal(2, messages.Count);
        Assert.Equal("m3", messages[0].Text);
        Assert.Equal("m4", messages[1].Text);
    }

    [SkippableFact]
    public async Task Messages_AreScopedToTheirConversation()
    {
        var store = await CreateStoreAsync();
        var a = await store.CreateSessionAsync("", null, null);
        var b = await store.CreateSessionAsync("", null, null);

        await store.AppendMessageAsync(a.ConversationId, MessageDirection.Inbound, "for A");
        await store.AppendMessageAsync(b.ConversationId, MessageDirection.Inbound, "for B");

        var aMessages = await store.ListMessagesAsync(a.ConversationId, 50);
        Assert.Single(aMessages);
        Assert.Equal("for A", aMessages[0].Text);
    }

    [SkippableFact]
    public async Task WorkflowStep_DefaultsNull_ThenUpsertsAndScopesByConversation()
    {
        var store = await CreateStoreAsync();
        var a = await store.CreateSessionAsync("", null, null);
        var b = await store.CreateSessionAsync("", null, null);

        // Fresh conversation → no step recorded.
        Assert.Null(await store.GetWorkflowStepAsync(a.ConversationId));

        await store.SetWorkflowStepAsync(a.ConversationId, "greet");
        Assert.Equal("greet", await store.GetWorkflowStepAsync(a.ConversationId));

        // Upsert replaces (no duplicate row / stale read).
        await store.SetWorkflowStepAsync(a.ConversationId, "qualify");
        Assert.Equal("qualify", await store.GetWorkflowStepAsync(a.ConversationId));

        // Scoped per conversation.
        Assert.Null(await store.GetWorkflowStepAsync(b.ConversationId));
    }

    [SkippableFact]
    public async Task SessionAuthenticated_DefaultsFalse_ThenUpsertsAndScopesByConversation()
    {
        var store = await CreateStoreAsync();
        var a = await store.CreateSessionAsync("", null, null);
        var b = await store.CreateSessionAsync("", null, null);

        // Fresh conversation → not verified (fail closed).
        Assert.False(await store.GetSessionAuthenticatedAsync(a.ConversationId));

        await store.SetSessionAuthenticatedAsync(a.ConversationId, true);
        Assert.True(await store.GetSessionAuthenticatedAsync(a.ConversationId));

        // Upsert replaces (can be cleared back to false).
        await store.SetSessionAuthenticatedAsync(a.ConversationId, false);
        Assert.False(await store.GetSessionAuthenticatedAsync(a.ConversationId));

        // Scoped per conversation.
        await store.SetSessionAuthenticatedAsync(a.ConversationId, true);
        Assert.False(await store.GetSessionAuthenticatedAsync(b.ConversationId));
    }

    [SkippableFact]
    public async Task ResumeSession_KnownConversation_ReusesIdAndKeepsHistory()
    {
        var store = await CreateStoreAsync();
        var first = await store.CreateSessionAsync("agent", "Alice", null);
        await store.AppendMessageAsync(first.ConversationId, MessageDirection.Inbound, "prior turn");

        // Resuming binds a NEW session to the SAME conversation, preserving its log.
        var resumed = await store.ResumeSessionAsync("agent", "Alice", null, first.ConversationId);
        Assert.Equal(first.ConversationId, resumed.ConversationId);
        Assert.NotEqual(first.SessionId, resumed.SessionId);
        Assert.Single(await store.ListMessagesAsync(resumed.ConversationId, 50));

        // A resumed session is a real, fetchable session.
        Assert.NotNull(await store.GetSessionAsync(resumed.SessionId));
    }

    [SkippableFact]
    public async Task ResumeSession_UnknownOrEmptyConversation_MintsFresh()
    {
        var store = await CreateStoreAsync();

        var unknown = await store.ResumeSessionAsync("agent", null, null, Guid.NewGuid().ToString());
        Assert.True(Guid.TryParse(unknown.ConversationId, out _));

        var empty = await store.ResumeSessionAsync("agent", null, null, null);
        Assert.True(Guid.TryParse(empty.ConversationId, out _));
        Assert.NotEqual(unknown.ConversationId, empty.ConversationId);
    }

    [SkippableFact]
    public async Task ListConversations_OnlyNonEmpty_WithCountAndFirstInbound()
    {
        var store = await CreateStoreAsync();

        // Empty conversation → excluded.
        await store.CreateSessionAsync("agent", null, null);
        // Non-empty conversation → one summary, first inbound captured.
        var withMessages = await store.CreateSessionAsync("agent", null, null);
        await store.AppendMessageAsync(withMessages.ConversationId, MessageDirection.Inbound, "first user line");
        await store.AppendMessageAsync(withMessages.ConversationId, MessageDirection.Outbound, "agent reply");

        var summaries = await store.ListConversationsAsync();
        var summary = Assert.Single(summaries, s => s.ConversationId == withMessages.ConversationId);
        Assert.Equal(2, summary.MessageCount);
        Assert.Equal("first user line", summary.FirstInboundText);
        Assert.DoesNotContain(summaries, s => s.ConversationId != withMessages.ConversationId && s.MessageCount == 0);
    }

    [SkippableFact]
    public async Task CreateSession_CapturesUserEmail_ForOtpContact()
    {
        var store = await CreateStoreAsync();

        var withEmail = await store.CreateSessionAsync("", "Alice", "alice@example.com");
        Assert.Equal("alice@example.com", (await store.GetSessionAsync(withEmail.SessionId))!.UserEmail);

        var withoutEmail = await store.CreateSessionAsync("", null, null);
        Assert.Null((await store.GetSessionAsync(withoutEmail.SessionId))!.UserEmail);
    }
}

/// <summary>The contract, against the in-memory adapter (always runs — no Docker).</summary>
public sealed class InMemorySessionStoreContractTests : SessionStoreContractTests
{
    protected override Task<ISessionStore> CreateStoreAsync() => Task.FromResult<ISessionStore>(new InMemorySessionStore());
}
