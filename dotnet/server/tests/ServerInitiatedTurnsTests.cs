using System.Text.Json.Nodes;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// The host-callable server-initiated turn seam (<see cref="IServerInitiatedTurns"/>): a turn started
/// server-side (e.g. a webhook → "investigate this alert") must create a conversation, run through the
/// SAME <see cref="TurnRunner"/> the client path uses, and persist events into the store so a client
/// that later lists or resumes that conversation sees it identically to a client-initiated turn.
/// </summary>
public class ServerInitiatedTurnsTests
{
    private static ServerInitiatedTurns Build(MockChatClient chat, InMemorySessionStore store) =>
        new(chat, store);

    [Fact]
    public async Task StartTurn_CreatesConversation_RunsTurn_AndReturnsIds()
    {
        var store = new InMemorySessionStore();
        var turns = Build(new MockChatClient().PushText("Investigating the alert now."), store);

        var result = await turns.StartTurnAsync("agent-1", "Datadog: p99 latency spiking on api-prime");

        Assert.False(string.IsNullOrEmpty(result.ConversationId));
        Assert.False(string.IsNullOrEmpty(result.SessionId));
        Assert.Equal("Investigating the alert now.", result.Turn.Reply);
        Assert.False(string.IsNullOrEmpty(result.Turn.MessageId));

        // The minted session is a real, retrievable session bound to the returned conversation.
        var session = await store.GetSessionAsync(result.SessionId);
        Assert.NotNull(session);
        Assert.Equal(result.ConversationId, session!.ConversationId);
        Assert.Equal("agent-1", session.AgentId);
    }

    [Fact]
    public async Task StartTurn_PersistsInboundAndOutbound_LikeAClientTurn()
    {
        var store = new InMemorySessionStore();
        var turns = Build(new MockChatClient().PushText("On it."), store);

        var result = await turns.StartTurnAsync("agent-1", "investigate this");

        // The message log is the durable surface a client reads — same shape as a client-initiated turn:
        // inbound user message, then the agent's outbound reply, in order.
        var messages = await store.ListMessagesAsync(result.ConversationId, 50);
        Assert.Equal(2, messages.Count);
        Assert.Equal(MessageDirection.Inbound, messages[0].Direction);
        Assert.Equal("investigate this", messages[0].Text);
        Assert.Equal(MessageDirection.Outbound, messages[1].Direction);
        Assert.Equal("On it.", messages[1].Text);
        // The returned MessageId is the persisted outbound row.
        Assert.Equal(messages[1].Id, result.Turn.MessageId);
    }

    [Fact]
    public async Task StartTurn_ConversationShowsInSidebarList()
    {
        var store = new InMemorySessionStore();
        var turns = Build(new MockChatClient().PushText("Reply."), store);

        var result = await turns.StartTurnAsync("agent-1", "the initiating question");

        // list_conversations (the resume/sidebar substrate) must surface the server-initiated
        // conversation exactly like a client one: present, with a title from the first inbound message.
        var summaries = await store.ListConversationsAsync(ConversationScope.Unscoped);
        var summary = Assert.Single(summaries, s => s.ConversationId == result.ConversationId);
        Assert.Equal(2, summary.MessageCount);
        Assert.Equal("the initiating question", summary.FirstInboundText);
    }

    [Fact]
    public async Task StartTurn_StreamsEventsToSink_WhenProvided()
    {
        var store = new InMemorySessionStore();
        var turns = Build(new MockChatClient().PushText("streamed reply"), store);

        var events = new List<JsonObject>();
        var result = await turns.StartTurnAsync("agent-1", "go", sink: events.Add, requestId: "req-9");

        // A host that supplies a sink gets the same stream_token events the socket path emits, stamped
        // with its requestId — so it can forward them live.
        Assert.Contains(events, e => e["type"]!.GetValue<string>() == "stream_token");
        Assert.All(events, e => Assert.Equal("req-9", e["requestId"]!.GetValue<string>()));
        Assert.Equal("streamed reply", result.Turn.Reply);
    }

    [Fact]
    public async Task StartTurn_WithoutSink_StillPersists()
    {
        var store = new InMemorySessionStore();
        var turns = Build(new MockChatClient().PushText("no sink needed"), store);

        // No sink: live events are discarded, but the message log is still written (the durable surface).
        var result = await turns.StartTurnAsync("agent-1", "fire and persist");

        var messages = await store.ListMessagesAsync(result.ConversationId, 50);
        Assert.Equal(2, messages.Count);
        Assert.Equal("no sink needed", messages[1].Text);
    }

    [Fact]
    public async Task StartTurn_ResultIsResumableByAClient_Identically()
    {
        var store = new InMemorySessionStore();
        var turns = Build(new MockChatClient().PushText("server said this"), store);

        var result = await turns.StartTurnAsync("agent-1", "server started this");

        // A client resuming the server-initiated conversation binds a new session to the same id and
        // sees the persisted history — indistinguishable from resuming a client-started conversation.
        var resumed = await store.ResumeSessionAsync("agent-1", null, null, result.ConversationId);
        Assert.Equal(result.ConversationId, resumed.ConversationId);
        Assert.NotEqual(result.SessionId, resumed.SessionId);

        var messages = await store.ListMessagesAsync(resumed.ConversationId, 50);
        Assert.Equal(2, messages.Count);
        Assert.Equal("server started this", messages[0].Text);
        Assert.Equal("server said this", messages[1].Text);
    }

    [Fact]
    public async Task StartTurn_EmptyAgentId_MintsAgent_LikeClientPath()
    {
        var store = new InMemorySessionStore();
        var turns = Build(new MockChatClient().PushText("ok"), store);

        var result = await turns.StartTurnAsync(string.Empty, "hello");

        var session = await store.GetSessionAsync(result.SessionId);
        Assert.NotNull(session);
        Assert.False(string.IsNullOrEmpty(session!.AgentId)); // store minted one, matching CreateSession.
    }
}
