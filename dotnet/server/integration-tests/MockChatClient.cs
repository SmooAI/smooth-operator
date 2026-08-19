using System.Runtime.CompilerServices;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Server.IntegrationTests;

/// <summary>A minimal scripted streaming <see cref="IChatClient"/> for the integration tests.</summary>
internal sealed class MockChatClient : IChatClient
{
    private readonly Queue<ChatResponse> _responses = new();

    public MockChatClient PushText(string text)
    {
        _responses.Enqueue(new ChatResponse(new ChatMessage(ChatRole.Assistant, text))
        {
            Usage = new UsageDetails { InputTokenCount = 10, OutputTokenCount = 5, TotalTokenCount = 15 },
            ModelId = "mock-model",
        });
        return this;
    }

    public MockChatClient PushToolCall(string callId, string name, IDictionary<string, object?> arguments)
    {
        _responses.Enqueue(new ChatResponse(new ChatMessage(ChatRole.Assistant, new List<AIContent> { new FunctionCallContent(callId, name, arguments) }))
        {
            ModelId = "mock-model",
        });
        return this;
    }

    /// <summary>How many model calls the server actually made (a cancelled turn must make none).</summary>
    public int Calls;

    private ChatResponse Next()
    {
        Interlocked.Increment(ref Calls);
        return _responses.Count > 0 ? _responses.Dequeue() : new ChatResponse(new ChatMessage(ChatRole.Assistant, string.Empty));
    }

    public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default) =>
        Task.FromResult(Next());

    public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
        IEnumerable<ChatMessage> messages,
        ChatOptions? options = null,
        [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        foreach (var update in Next().ToChatResponseUpdates())
        {
            await Task.Yield();
            yield return update;
        }
    }

    public object? GetService(Type serviceType, object? serviceKey = null) => null;

    public void Dispose()
    {
    }
}
