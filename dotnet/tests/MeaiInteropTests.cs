// Microsoft.Extensions.AI facade behaviour, driven through a mock transport — no
// live gateway. Covers the key interop paths:
//   • GetStreamingResponseAsync maps stream_token×N + eventual_response to ordered
//     ChatResponseUpdate text deltas, with the final text completing the stream.
//   • GetResponseAsync returns a ChatResponse carrying the assistant reply text.
//   • AddSmoothAgent registers an IChatClient resolving to SmoothAgentChatClient.

using Microsoft.Extensions.AI;
using Microsoft.Extensions.DependencyInjection;

namespace SmooAI.SmoothOperator.Tests;

public sealed class MeaiInteropTests
{
    private static string Frame(string template, string requestId) => template.Replace("{rid}", requestId);

    private static (SmoothAgentClient Client, MockTransport Transport, SmoothAgentChatClient Chat) MakeChat(SmoothAgentThread? thread = null)
    {
        var transport = new MockTransport();
        var counter = 0;
        var options = new SmoothAgentOptions
        {
            Url = "wss://test",
            AgentId = "agent-1",
            Transport = transport,
            GenerateRequestId = () => $"req-test-{++counter}",
            RequestTimeout = TimeSpan.FromSeconds(2),
        };
        var client = new SmoothAgentClient(new SmoothAgentClientOptions
        {
            Url = options.Url,
            Transport = options.Transport,
            GenerateRequestId = options.GenerateRequestId,
            RequestTimeout = options.RequestTimeout,
        });
        var chat = new SmoothAgentChatClient(client, options, thread);
        return (client, transport, chat);
    }

    /// <summary>Bind the chat client to a known session so no implicit create_session is needed.</summary>
    private static SmoothAgentThread BoundThread(SmoothAgentClient client)
        => new(client, "sess-1", "conv-1", "agent-1");

    [Fact]
    public async Task GetStreamingResponseAsync_MapsStreamTokensToOrderedTextDeltas_AndCompletesWithFinal()
    {
        var (client, transport, _) = MakeChat();
        await client.ConnectAsync();
        var chat = new SmoothAgentChatClient(client, new SmoothAgentOptions { Url = "wss://test", AgentId = "agent-1" }, BoundThread(client));

        var updates = new List<ChatResponseUpdate>();
        var iterate = Task.Run(async () =>
        {
            await foreach (var u in chat.GetStreamingResponseAsync(new[] { new ChatMessage(ChatRole.User, "hi") }))
                updates.Add(u);
        });

        // Wait for the send_message frame to go out, then drive a scripted sequence.
        await WaitFor(() => transport.Sent.Count >= 1);
        var reqId = transport.LastRequestId();
        var sent = transport.LastSent();
        Assert.Equal("send_message", sent.GetProperty("action").GetString());
        Assert.Equal("sess-1", sent.GetProperty("sessionId").GetString());
        Assert.Equal("hi", sent.GetProperty("message").GetString());

        transport.Emit(Frame("""{"type":"stream_token","requestId":"{rid}","token":"Hel","data":{"requestId":"{rid}","token":"Hel"}}""", reqId));
        transport.Emit(Frame("""{"type":"stream_token","requestId":"{rid}","token":"lo ","data":{"requestId":"{rid}","token":"lo "}}""", reqId));
        transport.Emit(Frame("""{"type":"stream_token","requestId":"{rid}","token":"world","data":{"requestId":"{rid}","token":"world"}}""", reqId));
        // Terminal text equals the concatenation of the deltas; the final update should add no duplicate text.
        transport.Emit(Frame("""{"type":"eventual_response","requestId":"{rid}","status":200,"data":{"requestId":"{rid}","status":200,"data":{"messageId":"msg-1","response":{"responseParts":["Hello world"]},"needsEscalation":false}}}""", reqId));

        await iterate;

        // Text deltas arrived in order.
        Assert.Equal(new[] { "Hel", "lo ", "world" },
            updates.Where(u => u.RawRepresentation is null).Select(u => u.Text).ToArray());

        // Full text = ordered concatenation of all update text (deltas + final remainder).
        var full = string.Concat(updates.Select(u => u.Text));
        Assert.Equal("Hello world", full);

        // The terminal update carries the message id and a Stop finish reason.
        var final = updates[^1];
        Assert.Equal("msg-1", final.MessageId);
        Assert.Equal(ChatFinishReason.Stop, final.FinishReason);
    }

    [Fact]
    public async Task GetStreamingResponseAsync_NoTokenDeltas_EmitsFullReplyAsSingleFinalUpdate()
    {
        var (client, transport, _) = MakeChat();
        await client.ConnectAsync();
        var chat = new SmoothAgentChatClient(client, new SmoothAgentOptions { AgentId = "agent-1" }, BoundThread(client));

        var updates = new List<ChatResponseUpdate>();
        var iterate = Task.Run(async () =>
        {
            await foreach (var u in chat.GetStreamingResponseAsync(new[] { new ChatMessage(ChatRole.User, "q") }))
                updates.Add(u);
        });

        await WaitFor(() => transport.Sent.Count >= 1);
        var reqId = transport.LastRequestId();
        transport.Emit(Frame("""{"type":"eventual_response","requestId":"{rid}","status":200,"data":{"requestId":"{rid}","status":200,"data":{"messageId":"m","response":{"responseParts":["The whole answer."]}}}}""", reqId));

        await iterate;

        Assert.Single(updates);
        Assert.Equal("The whole answer.", updates[0].Text);
    }

    [Fact]
    public async Task GetResponseAsync_ReturnsChatResponseWithReplyText()
    {
        var (client, transport, _) = MakeChat();
        await client.ConnectAsync();
        var chat = new SmoothAgentChatClient(client, new SmoothAgentOptions { AgentId = "agent-1" }, BoundThread(client));

        var task = chat.GetResponseAsync(new[] { new ChatMessage(ChatRole.User, "ping") });

        await WaitFor(() => transport.Sent.Count >= 1);
        var reqId = transport.LastRequestId();
        transport.Emit(Frame("""{"type":"stream_token","requestId":"{rid}","token":"ignored","data":{"requestId":"{rid}","token":"ignored"}}""", reqId));
        transport.Emit(Frame("""{"type":"eventual_response","requestId":"{rid}","status":200,"data":{"requestId":"{rid}","status":200,"data":{"messageId":"msg-7","response":{"responseParts":["Pong","there"]}}}}""", reqId));

        var response = await task;
        Assert.Equal("Pong there", response.Text);
        Assert.Equal("msg-7", response.ResponseId);
        Assert.Equal("conv-1", response.ConversationId);
        Assert.Equal(ChatRole.Assistant, response.Messages[0].Role);
    }

    [Fact]
    public async Task GetResponseAsync_CreatesSessionImplicitlyWhenNoThreadSupplied()
    {
        var (client, transport, chat) = MakeChat(); // no thread → must create one
        await client.ConnectAsync();

        var task = chat.GetResponseAsync(new[] { new ChatMessage(ChatRole.User, "hello") });

        // First frame must be a create_conversation_session.
        await WaitFor(() => transport.Sent.Count >= 1);
        var createReqId = transport.LastRequestId();
        var createFrame = transport.LastSent();
        Assert.Equal("create_conversation_session", createFrame.GetProperty("action").GetString());
        Assert.Equal("agent-1", createFrame.GetProperty("agentId").GetString());

        transport.Emit(Frame("""{"type":"immediate_response","requestId":"{rid}","status":200,"data":{"sessionId":"sess-new","conversationId":"conv-new","agentId":"agent-1","agentName":"Aria","userParticipantId":"u","agentParticipantId":"a"}}""", createReqId));

        // Then a send_message on the new session.
        await WaitFor(() => transport.Sent.Count >= 2);
        var sendReqId = transport.LastRequestId();
        var sendFrame = transport.LastSent();
        Assert.Equal("send_message", sendFrame.GetProperty("action").GetString());
        Assert.Equal("sess-new", sendFrame.GetProperty("sessionId").GetString());

        transport.Emit(Frame("""{"type":"eventual_response","requestId":"{rid}","status":200,"data":{"requestId":"{rid}","status":200,"data":{"messageId":"m","response":"Hi!"}}}""", sendReqId));

        var response = await task;
        Assert.Equal("Hi!", response.Text);
        Assert.NotNull(chat.Thread);
        Assert.Equal("sess-new", chat.Thread!.SessionId);
    }

    [Fact]
    public async Task AddSmoothAgent_RegistersIChatClientAsSmoothAgentChatClient()
    {
        var transport = new MockTransport();
        var services = new ServiceCollection();
        services.AddSmoothAgent(o =>
        {
            o.Url = "wss://test";
            o.AgentId = "agent-1";
            o.Transport = transport;
        });

        // SmoothAgentClient is IAsyncDisposable, so the provider must be disposed async.
        await using var provider = services.BuildServiceProvider();

        var chatClient = provider.GetRequiredService<IChatClient>();
        Assert.IsType<SmoothAgentChatClient>(chatClient);

        // The facade and the underlying client are the registered singletons.
        Assert.Same(chatClient, provider.GetRequiredService<SmoothAgentChatClient>());
        Assert.NotNull(provider.GetRequiredService<SmoothAgentClient>());

        // GetService surfaces metadata + the wrapped client.
        var metadata = chatClient.GetService(typeof(ChatClientMetadata)) as ChatClientMetadata;
        Assert.NotNull(metadata);
        Assert.Equal("smooth-operator", metadata!.ProviderName);
        Assert.Equal("agent-1", metadata.DefaultModelId);
    }

    [Fact]
    public async Task SmoothAgentThread_RunStreamingAsync_SendsOnBoundSession()
    {
        var (client, transport, _) = MakeChat();
        await client.ConnectAsync();
        var thread = new SmoothAgentThread(client, "sess-xyz", "conv-xyz", "agent-1");

        var turn = thread.RunStreamingAsync("hey");
        var reqId = transport.LastRequestId();
        var sent = transport.LastSent();
        Assert.Equal("send_message", sent.GetProperty("action").GetString());
        Assert.Equal("sess-xyz", sent.GetProperty("sessionId").GetString());
        Assert.True(sent.GetProperty("stream").GetBoolean());

        transport.Emit(Frame("""{"type":"eventual_response","requestId":"{rid}","status":200,"data":{"requestId":"{rid}","status":200,"data":{"messageId":"m","response":"ok"}}}""", reqId));
        var final = await turn.Completion;
        Assert.NotNull(final);
        Assert.Equal("m", final!.Data.Payload.MessageId);
    }

    /// <summary>Spin until <paramref name="predicate"/> is true or a short timeout elapses.</summary>
    private static async Task WaitFor(Func<bool> predicate, int timeoutMs = 2000)
    {
        var deadline = DateTime.UtcNow.AddMilliseconds(timeoutMs);
        while (!predicate())
        {
            if (DateTime.UtcNow > deadline)
                throw new TimeoutException("Condition not met in time.");
            await Task.Delay(10);
        }
    }
}
