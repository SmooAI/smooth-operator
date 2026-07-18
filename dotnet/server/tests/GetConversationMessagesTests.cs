using System.Text.Json.Nodes;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Tests for the <c>get_conversation_messages</c> WS action (pearl th-30a8a7) — newest-first paged
/// history per <c>spec/actions/get-messages.schema.json</c>. C# parity of the Rust
/// <c>handle_get_conversation_messages</c>.
/// </summary>
public class GetConversationMessagesTests
{
    private static (FrameDispatcher Dispatcher, InMemorySessionStore Store, List<JsonObject> Events) Build()
    {
        var store = new InMemorySessionStore();
        return (new FrameDispatcher(store, new MockChatClient()), store, new List<JsonObject>());
    }

    [Fact]
    public async Task ReturnsMessagesNewestFirstInDocumentedShape()
    {
        var (dispatcher, store, events) = Build();
        var session = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "hello");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Outbound, "hi back");

        var frame = $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{session.SessionId}}"}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("immediate_response", ev["type"]!.GetValue<string>());
        Assert.Equal(200, ev["status"]!.GetValue<int>());
        Assert.False(ev["data"]!["hasMore"]!.GetValue<bool>());

        var messages = ev["data"]!["messages"]!.AsArray();
        Assert.Equal(2, messages.Count);

        // Newest first: the outbound reply precedes the inbound message that prompted it.
        Assert.Equal("outbound", messages[0]!["direction"]!.GetValue<string>());
        Assert.Equal("hi back", messages[0]!["content"]!["text"]!.GetValue<string>());
        Assert.Equal("inbound", messages[1]!["direction"]!.GetValue<string>());
        Assert.Equal("hello", messages[1]!["content"]!["text"]!.GetValue<string>());

        foreach (var m in messages)
        {
            Assert.False(string.IsNullOrEmpty(m!["id"]!.GetValue<string>()));
            // createdAt round-trips as ISO 8601.
            Assert.True(DateTimeOffset.TryParse(m["createdAt"]!.GetValue<string>(), out _));
        }
    }

    [Fact]
    public async Task UnknownSession_ReturnsSessionNotFound()
    {
        var (dispatcher, _, events) = Build();
        var frame = $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{Guid.NewGuid()}}"}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("error", ev["type"]!.GetValue<string>());
        Assert.Equal("SESSION_NOT_FOUND", ev["error"]!["code"]!.GetValue<string>());
    }

    [Fact]
    public async Task MissingSessionId_ReturnsValidationError()
    {
        var (dispatcher, _, events) = Build();
        await dispatcher.DispatchAsync("""{"action":"get_conversation_messages","requestId":"r1"}""", events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("error", ev["type"]!.GetValue<string>());
        Assert.Equal("VALIDATION_ERROR", ev["error"]!["code"]!.GetValue<string>());
    }

    [Fact]
    public async Task LimitSmallerThanHistory_TrimsAndSetsHasMore()
    {
        var (dispatcher, store, events) = Build();
        var session = await store.CreateSessionAsync("agent", "U", "u@example.com");
        for (var i = 0; i < 5; i++)
        {
            await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, $"msg {i}");
        }

        var frame = $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{session.SessionId}}","limit":2}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var data = Assert.Single(events)["data"]!;
        Assert.True(data["hasMore"]!.GetValue<bool>());
        var messages = data["messages"]!.AsArray();
        Assert.Equal(2, messages.Count);
        Assert.Equal("msg 4", messages[0]!["content"]!["text"]!.GetValue<string>());
        Assert.Equal("msg 3", messages[1]!["content"]!["text"]!.GetValue<string>());
    }

    [Fact]
    public async Task LimitOutOfRange_IsClamped()
    {
        var (dispatcher, store, events) = Build();
        var session = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "a");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "b");

        // 0 clamps up to 1 (never "return nothing"), so exactly the newest message comes back.
        var frame = $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{session.SessionId}}","limit":0}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var data = Assert.Single(events)["data"]!;
        Assert.Single(data["messages"]!.AsArray());
        Assert.True(data["hasMore"]!.GetValue<bool>());
    }

    [Fact]
    public async Task Cursor_ReturnsOnlyMessagesOlderThanTheOneItNames()
    {
        var (dispatcher, store, events) = Build();
        var session = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "old");
        var newer = await store.AppendMessageAsync(session.ConversationId, MessageDirection.Outbound, "new");

        var frame = $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{session.SessionId}}","cursor":"{{newer.Id}}"}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var data = Assert.Single(events)["data"]!;
        var only = Assert.Single(data["messages"]!.AsArray());
        Assert.Equal("old", only!["content"]!["text"]!.GetValue<string>());
        Assert.False(data["hasMore"]!.GetValue<bool>());
        Assert.Null(data["nextCursor"]);
    }

    [Fact]
    public async Task NextCursor_NamesOldestOfPage_AndIsNullOnLastPage()
    {
        var (dispatcher, store, events) = Build();
        var session = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "a");
        var b = await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "b");

        var frame = $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{session.SessionId}}","limit":1}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var data = Assert.Single(events)["data"]!;
        Assert.True(data["hasMore"]!.GetValue<bool>());
        Assert.Equal(b.Id, data["nextCursor"]!.GetValue<string>());
    }

    [Fact]
    public async Task RoundTripPaging_WalksEveryMessageExactlyOnce()
    {
        var (dispatcher, store, events) = Build();
        var session = await store.CreateSessionAsync("agent", "U", "u@example.com");
        for (var i = 0; i < 4; i++)
        {
            await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, $"msg {i}");
        }

        var seen = await DrainPagesAsync(dispatcher, session.SessionId, events);
        Assert.Equal(new[] { "msg 3", "msg 2", "msg 1", "msg 0" }, seen);
    }

    /// <summary>
    /// The case a timestamp cursor cannot pass: two messages sharing an identical <c>CreatedAt</c>,
    /// walked one per page. A <c>created_at &lt; cursor</c> filter drops or repeats the collision;
    /// an id cursor names exactly one message. th-f63e4b.
    /// </summary>
    [Fact]
    public async Task IdenticalTimestamps_ArePagedWithoutLossOrDuplication()
    {
        var inner = new InMemorySessionStore();
        var store = new FrozenClockStore(inner);
        var dispatcher = new FrameDispatcher(store, new MockChatClient());
        var events = new List<JsonObject>();

        var session = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "twin a");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Outbound, "twin b");

        // Both messages report the same createdAt, so ordering can only come from the store's own order.
        var stamps = (await store.ListMessagesAsync(session.ConversationId, 10)).Select(m => m.CreatedAt).Distinct();
        Assert.Single(stamps);

        var seen = await DrainPagesAsync(dispatcher, session.SessionId, events);
        Assert.Equal(new[] { "twin b", "twin a" }, seen);
    }

    [Fact]
    public async Task UnknownCursor_ReturnsValidationError()
    {
        var (dispatcher, store, events) = Build();
        var session = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "only");

        var frame = $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{session.SessionId}}","cursor":"{{Guid.NewGuid()}}"}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("error", ev["type"]!.GetValue<string>());
        Assert.Equal("VALIDATION_ERROR", ev["error"]!["code"]!.GetValue<string>());
    }

    /// <summary>Page one message at a time following <c>nextCursor</c>, returning the texts in order.</summary>
    private static async Task<List<string>> DrainPagesAsync(FrameDispatcher dispatcher, string sessionId, List<JsonObject> events)
    {
        var seen = new List<string>();
        string? cursor = null;
        while (true)
        {
            events.Clear();
            var cursorField = cursor is null ? string.Empty : ",\"cursor\":\"" + cursor + "\"";
            await dispatcher.DispatchAsync(
                $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{sessionId}}","limit":1{{cursorField}}}""",
                events.Add);

            var data = Assert.Single(events)["data"]!;
            seen.AddRange(data["messages"]!.AsArray().Select(m => m!["content"]!["text"]!.GetValue<string>()));

            if (!data["hasMore"]!.GetValue<bool>())
            {
                Assert.Null(data["nextCursor"]);
                return seen;
            }
            cursor = data["nextCursor"]!.GetValue<string>();
            Assert.NotNull(cursor);
        }
    }

    /// <summary>Delegating store that reports one fixed <c>CreatedAt</c> for every message, so paging
    /// can be tested against a timestamp collision the real clock rarely produces.</summary>
    private sealed class FrozenClockStore(ISessionStore inner) : ISessionStore
    {
        private static readonly DateTimeOffset Frozen = new(2026, 1, 1, 0, 0, 0, TimeSpan.Zero);

        public Task<StoredSession> CreateSessionAsync(string agentId, string? userName, string? userEmail, CancellationToken cancellationToken = default) =>
            inner.CreateSessionAsync(agentId, userName, userEmail, cancellationToken);

        public Task<StoredSession> ResumeSessionAsync(string agentId, string? userName, string? userEmail, string? conversationId, CancellationToken cancellationToken = default) =>
            inner.ResumeSessionAsync(agentId, userName, userEmail, conversationId, cancellationToken);

        public Task<IReadOnlyList<ConversationSummary>> ListConversationsAsync(CancellationToken cancellationToken = default) =>
            inner.ListConversationsAsync(cancellationToken);

        public Task<StoredSession?> GetSessionAsync(string sessionId, CancellationToken cancellationToken = default) =>
            inner.GetSessionAsync(sessionId, cancellationToken);

        public async Task<StoredMessage> AppendMessageAsync(string conversationId, MessageDirection direction, string text, CancellationToken cancellationToken = default) =>
            (await inner.AppendMessageAsync(conversationId, direction, text, cancellationToken)) with { CreatedAt = Frozen };

        public async Task<IReadOnlyList<StoredMessage>> ListMessagesAsync(string conversationId, int limit, CancellationToken cancellationToken = default) =>
            (await inner.ListMessagesAsync(conversationId, limit, cancellationToken)).Select(m => m with { CreatedAt = Frozen }).ToList();

        public Task<string?> GetWorkflowStepAsync(string conversationId, CancellationToken cancellationToken = default) =>
            inner.GetWorkflowStepAsync(conversationId, cancellationToken);

        public Task SetWorkflowStepAsync(string conversationId, string stepId, CancellationToken cancellationToken = default) =>
            inner.SetWorkflowStepAsync(conversationId, stepId, cancellationToken);

        public Task<bool> GetSessionAuthenticatedAsync(string conversationId, CancellationToken cancellationToken = default) =>
            inner.GetSessionAuthenticatedAsync(conversationId, cancellationToken);

        public Task SetSessionAuthenticatedAsync(string conversationId, bool verified, CancellationToken cancellationToken = default) =>
            inner.SetSessionAuthenticatedAsync(conversationId, verified, cancellationToken);
    }

    [Fact]
    public async Task EmptyConversation_ReturnsEmptyPage()
    {
        var (dispatcher, store, events) = Build();
        var session = await store.CreateSessionAsync("agent", "U", "u@example.com");

        var frame = $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{session.SessionId}}"}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var data = Assert.Single(events)["data"]!;
        Assert.Empty(data["messages"]!.AsArray());
        Assert.False(data["hasMore"]!.GetValue<bool>());
    }
}
