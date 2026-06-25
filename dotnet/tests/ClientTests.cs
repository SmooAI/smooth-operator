// Client behaviour, driven entirely through a mock transport — no live server.
//
// Covers the core streaming contract: a send_message turn surfaces stream_token →
// stream_chunk → eventual_response as typed events in arrival order, and resolves
// the turn with the terminal response. Also covers request correlation for
// non-streaming actions, error propagation, and HITL resume.

using System.Text.Json;

namespace SmooAI.SmoothOperator.Tests;

public sealed class ClientTests
{
    /// <summary>Build a JSON frame, substituting the {rid} placeholder with the requestId.</summary>
    private static string Frame(string template, string requestId) => template.Replace("{rid}", requestId);

    private static (SmoothAgentClient Client, MockTransport Transport) MakeClient()
    {
        var transport = new MockTransport();
        var counter = 0;
        var client = new SmoothAgentClient(new SmoothAgentClientOptions
        {
            Url = "wss://test",
            Transport = transport,
            GenerateRequestId = () => $"req-test-{++counter}",
            RequestTimeout = TimeSpan.FromSeconds(1),
        });
        return (client, transport);
    }

    [Fact]
    public async Task SendMessage_SurfacesTokenThenChunkThenEventual_InOrder_AndResolves()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var turn = client.SendMessageAsync(new SendMessageAction
        {
            SessionId = "sess-1",
            Message = "hi",
            Stream = true,
        });
        var reqId = transport.LastRequestId();

        // The outgoing frame is a well-formed send_message action.
        var sent = transport.LastSent();
        Assert.Equal("send_message", sent.GetProperty("action").GetString());
        Assert.Equal("sess-1", sent.GetProperty("sessionId").GetString());
        Assert.Equal("hi", sent.GetProperty("message").GetString());

        // Collect streamed events via async iteration in a background task.
        var collected = new List<ServerEvent>();
        var iterate = Task.Run(async () =>
        {
            await foreach (var ev in turn)
                collected.Add(ev);
        });

        // Drive a realistic event sequence.
        transport.Emit(Frame("""{"type":"stream_token","requestId":"{rid}","token":"Hel","data":{"requestId":"{rid}","token":"Hel"}}""", reqId));
        transport.Emit(Frame("""{"type":"stream_token","requestId":"{rid}","token":"lo","data":{"requestId":"{rid}","token":"lo"}}""", reqId));
        transport.Emit(Frame("""{"type":"stream_chunk","requestId":"{rid}","node":"response_composer","data":{"requestId":"{rid}","node":"response_composer","state":{"structuredResponse":{"responseParts":["Hello"]}}}}""", reqId));
        transport.Emit(Frame("""{"type":"eventual_response","requestId":"{rid}","status":200,"data":{"requestId":"{rid}","status":200,"data":{"messageId":"msg-1","response":{"responseParts":["Hello"]},"needsEscalation":false}}}""", reqId));

        var final = await turn.Completion;
        await iterate;

        // Terminal response resolves the turn.
        Assert.Equal(EventTypes.EventualResponse, final.Type);
        Assert.Equal("msg-1", final.Data.Payload.MessageId);

        // Events arrived in order through iteration.
        Assert.Equal(
            new[] { "stream_token", "stream_token", "stream_chunk", "eventual_response" },
            collected.Select(e => e.Type).ToArray());

        var tokens = string.Concat(collected.OfType<StreamTokenEvent>().Select(e => e.Token));
        Assert.Equal("Hello", tokens);
    }

    [Fact]
    public async Task SendMessage_BuffersTokensPushedBeforeIterationBegins()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var turn = client.SendMessageAsync(new SendMessageAction { SessionId = "s", Message = "q" });
        var reqId = transport.LastRequestId();

        // Emit before anyone iterates — must be buffered.
        transport.Emit(Frame("""{"type":"stream_token","requestId":"{rid}","token":"A","data":{"requestId":"{rid}","token":"A"}}""", reqId));
        transport.Emit(Frame("""{"type":"eventual_response","requestId":"{rid}","status":200,"data":{"requestId":"{rid}","status":200,"data":{"messageId":"m","response":null}}}""", reqId));

        var types = new List<string>();
        await foreach (var ev in turn)
            types.Add(ev.Type);

        Assert.Equal(new[] { "stream_token", "eventual_response" }, types.ToArray());
    }

    [Fact]
    public async Task SendMessage_RejectsTurnOnErrorEvent_WithProtocolException()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var turn = client.SendMessageAsync(new SendMessageAction { SessionId = "s", Message = "boom" });
        var reqId = transport.LastRequestId();

        transport.Emit(Frame("""{"type":"error","requestId":"{rid}","data":{"requestId":"{rid}","error":{"code":"RATE_LIMITED","message":"slow down"}}}""", reqId));

        var ex = await Assert.ThrowsAsync<ProtocolException>(() => turn.Completion);
        Assert.Equal("RATE_LIMITED", ex.Code);
    }

    [Fact]
    public async Task SendMessage_RoutesHitlConfirmResumeBackIntoSameTurn()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var turn = client.SendMessageAsync(new SendMessageAction { SessionId = "s", Message = "delete it" });
        var reqId = transport.LastRequestId();

        var seen = new List<string>();
        var iterate = Task.Run(async () =>
        {
            await foreach (var ev in turn)
                seen.Add(ev.Type);
        });

        transport.Emit(Frame("""{"type":"write_confirmation_required","requestId":"{rid}","data":{"requestId":"{rid}","data":{"toolId":"t1","actionDescription":"Delete contact"}}}""", reqId));

        // Caller approves; the resumed stream completes the original turn.
        await client.ConfirmToolActionAsync("s", reqId, approved: true);
        var confirm = transport.LastSent();
        Assert.Equal("confirm_tool_action", confirm.GetProperty("action").GetString());
        Assert.True(confirm.GetProperty("approved").GetBoolean());
        Assert.Equal(reqId, confirm.GetProperty("requestId").GetString());

        transport.Emit(Frame("""{"type":"eventual_response","requestId":"{rid}","status":200,"data":{"requestId":"{rid}","status":200,"data":{"messageId":"m","response":null}}}""", reqId));

        await turn.Completion;
        await iterate;
        Assert.Equal(new[] { "write_confirmation_required", "eventual_response" }, seen.ToArray());
    }

    [Fact]
    public async Task CreateConversationSession_ResolvesWithImmediateResponseData()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var task = client.CreateConversationSessionAsync(new CreateConversationSessionAction
        {
            AgentId = "agent-1",
            UserName = "Alice",
        });
        var reqId = transport.LastRequestId();
        var sent = transport.LastSent();
        Assert.Equal("create_conversation_session", sent.GetProperty("action").GetString());
        Assert.Equal("agent-1", sent.GetProperty("agentId").GetString());

        transport.Emit(Frame("""{"type":"immediate_response","requestId":"{rid}","status":200,"data":{"sessionId":"sess-9","conversationId":"conv-9","agentId":"agent-1","agentName":"Aria","userParticipantId":"u-9","agentParticipantId":"a-9"}}""", reqId));

        var session = await task;
        Assert.Equal("sess-9", session.SessionId);
        Assert.Equal("Aria", session.AgentName);
    }

    [Fact]
    public async Task Ping_ResolvesWithPongTimestamp()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var task = client.PingAsync();
        var reqId = transport.LastRequestId();
        transport.Emit(Frame("""{"type":"pong","requestId":"{rid}","timestamp":1700000000000}""", reqId));

        Assert.Equal(1700000000000L, await task);
    }

    [Fact]
    public async Task DoesNotCrossCorrelateTwoConcurrentRequests()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var p1 = client.GetSessionAsync(new GetSessionAction { SessionId = "s1" });
        var req1 = transport.LastRequestId();
        var p2 = client.GetSessionAsync(new GetSessionAction { SessionId = "s2" });
        var req2 = transport.LastRequestId();
        Assert.NotEqual(req1, req2);

        static string SessionData(string reqId, string id) =>
            Frame("""{"type":"immediate_response","requestId":"{rid}","status":200,"data":{"sessionId":"{sid}","conversationId":"c","agentId":"a","agentName":"N","userParticipantId":"u","agentParticipantId":"ag"}}""", reqId)
                .Replace("{sid}", id);

        // Resolve out of order.
        transport.Emit(SessionData(req2, "s2"));
        transport.Emit(SessionData(req1, "s1"));

        Assert.Equal("s1", (await p1).SessionId);
        Assert.Equal("s2", (await p2).SessionId);
    }

    [Fact]
    public async Task ForwardsUncorrelatedKeepaliveToEventListeners()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var received = new List<ServerEvent>();
        client.Event += ev => received.Add(ev);

        transport.Emit("""{"type":"keepalive","data":{"requestId":"whatever"}}""");

        Assert.Single(received);
        Assert.Equal(EventTypes.Keepalive, received[0].Type);
    }

    [Fact]
    public async Task RejectsPendingRequestsWhenTransportCloses()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var task = client.GetSessionAsync(new GetSessionAction { SessionId = "s" });
        await transport.CloseAsync();

        await Assert.ThrowsAnyAsync<Exception>(() => task);
    }

    [Fact]
    public void TokenOption_AddsTokenQueryToConnectUri()
    {
        var merged = SmoothAgentClient.WithToken("wss://realtime.test/ws", "secret123");

        Assert.Contains("token=secret123", new Uri(merged).Query);
    }

    [Fact]
    public void TokenOption_PreservesExistingQuery()
    {
        var merged = SmoothAgentClient.WithToken("wss://realtime.test/ws?foo=bar", "secret123");

        var query = System.Web.HttpUtility.ParseQueryString(new Uri(merged).Query);
        Assert.Equal("bar", query["foo"]);
        Assert.Equal("secret123", query["token"]);
    }

    [Fact]
    public void TokenOption_PercentEncodesTokenValue()
    {
        var merged = SmoothAgentClient.WithToken("wss://realtime.test/ws", "a b/c+d");

        // The raw query is percent-encoded; decoding round-trips to the original value.
        Assert.DoesNotContain("a b/c+d", new Uri(merged).Query);
        var query = System.Web.HttpUtility.ParseQueryString(new Uri(merged).Query);
        Assert.Equal("a b/c+d", query["token"]);
    }

    [Fact]
    public void TokenOption_NoToken_LeavesUrlUnchanged()
    {
        Assert.Equal("wss://realtime.test/ws", SmoothAgentClient.WithToken("wss://realtime.test/ws", null));
        Assert.Equal("wss://realtime.test/ws", SmoothAgentClient.WithToken("wss://realtime.test/ws", ""));
    }
}
