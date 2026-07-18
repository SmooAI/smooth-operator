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
    public async Task BeforeCursor_ReturnsOnlyOlderMessages()
    {
        var (dispatcher, store, events) = Build();
        var session = await store.CreateSessionAsync("agent", "U", "u@example.com");
        var first = await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "old");
        await Task.Delay(10); // distinct wall-clock stamps so the cursor has something to cut on.
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Outbound, "new");

        var cursor = first.CreatedAt.AddMilliseconds(1).ToUniversalTime().ToString("O");
        var frame = $$"""{"action":"get_conversation_messages","requestId":"r1","sessionId":"{{session.SessionId}}","before":"{{cursor}}"}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var data = Assert.Single(events)["data"]!;
        var messages = data["messages"]!.AsArray();
        var only = Assert.Single(messages);
        Assert.Equal("old", only!["content"]!["text"]!.GetValue<string>());
        Assert.False(data["hasMore"]!.GetValue<bool>());
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
