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
}

/// <summary>The contract, against the in-memory adapter (always runs — no Docker).</summary>
public sealed class InMemorySessionStoreContractTests : SessionStoreContractTests
{
    protected override Task<ISessionStore> CreateStoreAsync() => Task.FromResult<ISessionStore>(new InMemorySessionStore());
}
