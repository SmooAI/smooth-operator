using System.Runtime.CompilerServices;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Captures the full message list each turn hands the model (for asserting attached content).
/// Shared across parity suites — <see cref="FileTransferTests"/> asserts on the attached image/file
/// parts, <see cref="SkillTests"/> on the composed system prompt.
/// </summary>
internal sealed class RecordingChatClient : IChatClient
{
    private readonly string _reply;

    public RecordingChatClient(string reply) => _reply = reply;

    public IReadOnlyList<ChatMessage> LastMessages { get; private set; } = Array.Empty<ChatMessage>();

    public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
    {
        LastMessages = messages.ToList();
        return Task.FromResult(new ChatResponse(new ChatMessage(ChatRole.Assistant, _reply)) { ModelId = "record" });
    }

    public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
        IEnumerable<ChatMessage> messages, ChatOptions? options = null, [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        LastMessages = messages.ToList();
        foreach (var update in new ChatResponse(new ChatMessage(ChatRole.Assistant, _reply)).ToChatResponseUpdates())
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
