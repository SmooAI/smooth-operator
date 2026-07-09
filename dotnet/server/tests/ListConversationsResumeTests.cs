using System.Text.Json.Nodes;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Tests for the conversation-list / resume substrate (pearl th-d5b446): the <c>list_conversations</c>
/// WS action + <c>create_conversation_session</c>'s optional <c>conversationId</c> resume. The C#
/// parity of the Go <c>conversations_test.go</c> and the TS list-conversations-resume test.
/// </summary>
public class ListConversationsResumeTests
{
    private static (FrameDispatcher Dispatcher, InMemorySessionStore Store, List<JsonObject> Events) Build()
    {
        var store = new InMemorySessionStore();
        return (new FrameDispatcher(store, new MockChatClient()), store, new List<JsonObject>());
    }

    // ---- ConversationTitle: preview cleaning + truncation ----------------------------------------

    [Theory]
    [InlineData("Hello there", "fb", "Hello there")]
    [InlineData("   spaced   ", "fb", "spaced")]
    [InlineData("### Big title", "fb", "Big title")]
    [InlineData("- do the thing", "fb", "do the thing")]
    [InlineData("> _quoted_ line", "fb", "quoted_ line")]
    [InlineData("", "My Conversation", "My Conversation")]
    [InlineData("###   ", "Fallback", "Fallback")]
    [InlineData(null, "NullFallback", "NullFallback")]
    public void ConversationTitle_CleansAndFallsBack(string? first, string fallback, string want)
    {
        Assert.Equal(want, FrameDispatcher.ConversationTitle(first, fallback));
    }

    [Fact]
    public void ConversationTitle_TruncatesLongTo60WithEllipsis()
    {
        var first = "012345678901234567890123456789012345678901234567890123456789EXTRA";
        var got = FrameDispatcher.ConversationTitle(first, "fb");
        Assert.Equal("012345678901234567890123456789012345678901234567890123456789…", got);
        // 60 visible chars + the ellipsis rune.
        Assert.Equal(61, got.EnumerateRunes().Count());
    }

    // ---- ListConversations: filter empties, sort, title ------------------------------------------

    [Fact]
    public async Task ListConversations_FiltersEmptyConversations()
    {
        var (dispatcher, store, events) = Build();

        // A: empty conversation (created, never messaged) → excluded.
        await store.CreateSessionAsync("agent", "U", "u@example.com");
        // B: has messages → included, title from first inbound.
        var b = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(b.ConversationId, MessageDirection.Inbound, "## First user line");
        await store.AppendMessageAsync(b.ConversationId, MessageDirection.Outbound, "agent reply");

        await dispatcher.DispatchAsync("""{"action":"list_conversations","requestId":"r1"}""", events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("immediate_response", ev["type"]!.GetValue<string>());
        Assert.Equal(200, ev["status"]!.GetValue<int>());
        var conversations = ev["data"]!["conversations"]!.AsArray();
        var only = Assert.Single(conversations);
        Assert.Equal(b.ConversationId, only!["conversationId"]!.GetValue<string>());
        Assert.Equal(2, only["messageCount"]!.GetValue<int>());
        Assert.Equal("First user line", only["title"]!.GetValue<string>()); // "## " stripped.
        Assert.False(string.IsNullOrEmpty(only["updatedAt"]!.GetValue<string>()));
    }

    [Fact]
    public async Task ListConversations_SortedMostRecentFirst()
    {
        var (dispatcher, store, events) = Build();

        var older = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(older.ConversationId, MessageDirection.Inbound, "older");
        var newer = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(newer.ConversationId, MessageDirection.Inbound, "newer");
        // Touch `older` again so it becomes the most recently active.
        await store.AppendMessageAsync(older.ConversationId, MessageDirection.Outbound, "older reply");

        await dispatcher.DispatchAsync("""{"action":"list_conversations","requestId":"r1"}""", events.Add);

        var conversations = Assert.Single(events)["data"]!["conversations"]!.AsArray();
        Assert.Equal(2, conversations.Count);
        Assert.Equal(older.ConversationId, conversations[0]!["conversationId"]!.GetValue<string>());
        Assert.Equal(newer.ConversationId, conversations[1]!["conversationId"]!.GetValue<string>());
    }

    [Fact]
    public async Task ListConversations_RespectsLimit()
    {
        var (dispatcher, store, events) = Build();
        for (var i = 0; i < 3; i++)
        {
            var s = await store.CreateSessionAsync("agent", "U", "u@example.com");
            await store.AppendMessageAsync(s.ConversationId, MessageDirection.Inbound, $"msg {i}");
        }

        await dispatcher.DispatchAsync("""{"action":"list_conversations","requestId":"r1","limit":2}""", events.Add);

        var conversations = Assert.Single(events)["data"]!["conversations"]!.AsArray();
        Assert.Equal(2, conversations.Count);
    }

    [Fact]
    public async Task ListConversations_EmptyStore_ReturnsEmptyArray()
    {
        var (dispatcher, _, events) = Build();
        await dispatcher.DispatchAsync("""{"action":"list_conversations","requestId":"r1"}""", events.Add);
        var conversations = Assert.Single(events)["data"]!["conversations"]!.AsArray();
        Assert.Empty(conversations);
    }

    // ---- create_conversation_session resume ------------------------------------------------------

    [Fact]
    public async Task CreateSession_WithKnownConversationId_Resumes()
    {
        var (dispatcher, store, events) = Build();

        // Seed a conversation with history.
        var original = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(original.ConversationId, MessageDirection.Inbound, "prior turn");

        // Resume by passing its conversationId — a NEW session bound to the SAME conversation.
        var frame = $$"""{"action":"create_conversation_session","requestId":"r1","conversationId":"{{original.ConversationId}}"}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var data = Assert.Single(events)["data"]!.AsObject();
        Assert.Equal(original.ConversationId, data["conversationId"]!.GetValue<string>()); // same conversation.
        Assert.NotEqual(original.SessionId, data["sessionId"]!.GetValue<string>());        // new session.

        // History is preserved (not reset by the resume).
        var messages = await store.ListMessagesAsync(original.ConversationId, 50);
        Assert.Single(messages);
        Assert.Equal("prior turn", messages[0].Text);
    }

    [Fact]
    public async Task CreateSession_WithUnknownConversationId_MintsFresh()
    {
        var (dispatcher, _, events) = Build();
        var unknown = Guid.NewGuid().ToString();

        var frame = $$"""{"action":"create_conversation_session","requestId":"r1","conversationId":"{{unknown}}"}""";
        await dispatcher.DispatchAsync(frame, events.Add);

        var data = Assert.Single(events)["data"]!.AsObject();
        Assert.NotEqual(unknown, data["conversationId"]!.GetValue<string>()); // fresh id, not the unknown one.
    }

    [Fact]
    public async Task CreateSession_WithoutConversationId_MintsFresh()
    {
        var (dispatcher, _, events) = Build();
        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","requestId":"r1"}""", events.Add);
        var data = Assert.Single(events)["data"]!.AsObject();
        Assert.False(string.IsNullOrEmpty(data["conversationId"]!.GetValue<string>()));
    }

    // ---- store-level resume semantics ------------------------------------------------------------

    [Fact]
    public async Task ResumeSession_KnownConversation_ReusesIdAndKeepsLog()
    {
        var store = new InMemorySessionStore();
        var first = await store.CreateSessionAsync("agent", "U", "u@example.com");
        await store.AppendMessageAsync(first.ConversationId, MessageDirection.Inbound, "hi");

        var resumed = await store.ResumeSessionAsync("agent", "U", "u@example.com", first.ConversationId);
        Assert.Equal(first.ConversationId, resumed.ConversationId);
        Assert.NotEqual(first.SessionId, resumed.SessionId);

        var messages = await store.ListMessagesAsync(resumed.ConversationId, 50);
        Assert.Single(messages); // log intact, not wiped.
    }
}
