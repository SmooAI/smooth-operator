using System.Runtime.CompilerServices;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Core.Tests;

/// <summary>
/// A scripted <see cref="IChatClient"/> test double — the C# analog of the Rust engine's
/// <c>MockLlmClient</c>. Queue up responses (text or tool calls); each call to the model
/// dequeues the next one. Records the messages it was called with so tests can assert the
/// user message / tool results reached the model. No network, fully deterministic.
/// </summary>
internal sealed class MockChatClient : IChatClient
{
    private readonly Queue<ChatResponse> _responses = new();

    /// <summary>The messages passed to the model on each call, in order.</summary>
    public List<IList<ChatMessage>> Calls { get; } = new();

    public int CallCount => Calls.Count;

    private static UsageDetails Tokens() => new() { InputTokenCount = 10, OutputTokenCount = 5, TotalTokenCount = 15 };

    /// <summary>Script a plain assistant text response (ends the loop).</summary>
    public MockChatClient PushText(string text)
    {
        _responses.Enqueue(new ChatResponse(new ChatMessage(ChatRole.Assistant, text)) { Usage = Tokens() });
        return this;
    }

    /// <summary>Script an assistant turn that requests a tool call (continues the loop).</summary>
    public MockChatClient PushToolCall(string callId, string name, IDictionary<string, object?> arguments)
    {
        var message = new ChatMessage(ChatRole.Assistant, new List<AIContent> { new FunctionCallContent(callId, name, arguments) });
        _responses.Enqueue(new ChatResponse(message) { Usage = Tokens() });
        return this;
    }

    public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
    {
        Calls.Add(messages.ToList());
        if (_responses.Count == 0)
        {
            throw new InvalidOperationException("MockChatClient: no scripted response left.");
        }
        return Task.FromResult(_responses.Dequeue());
    }

    public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
        IEnumerable<ChatMessage> messages,
        ChatOptions? options = null,
        [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        Calls.Add(messages.ToList());
        if (_responses.Count == 0)
        {
            throw new InvalidOperationException("MockChatClient: no scripted response left.");
        }
        var response = _responses.Dequeue();
        foreach (var update in response.ToChatResponseUpdates())
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
