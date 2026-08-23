using System.Text.Json;

namespace SmooAI.SmoothOperator.Tests;

/// <summary>In-memory transport: captures sent frames, lets the test inject server events.</summary>
internal sealed class MockTransport : ITransport
{
    public TransportState State { get; private set; } = TransportState.Closed;
    public List<string> Sent { get; } = new();

    public event Action<string>? Message;
    public event Action<TransportCloseInfo>? Closed;
    public event Action<Exception>? Error;

    public Task ConnectAsync(CancellationToken cancellationToken = default)
    {
        State = TransportState.Open;
        return Task.CompletedTask;
    }

    /// <summary>
    /// When set, <see cref="SendAsync"/> fails ASYNCHRONOUSLY with this exception — it faults
    /// the returned task instead of throwing at the call site. That is how the real
    /// WebSocketTransport behaves (its SendAsync is <c>async</c>), and it is the case a
    /// try/catch wrapped around a discarded <c>_ = SendAsync(...)</c> silently misses.
    /// </summary>
    public Exception? AsyncSendFailure { get; set; }

    public Task SendAsync(string data, CancellationToken cancellationToken = default)
    {
        if (AsyncSendFailure is not null)
            return FailAsync(AsyncSendFailure);
        if (State != TransportState.Open)
            throw new InvalidOperationException($"not open: {State}");
        Sent.Add(data);
        return Task.CompletedTask;
    }

    private static async Task FailAsync(Exception ex)
    {
        await Task.Yield();
        throw ex;
    }

    public Task CloseAsync(int code = 1000, string? reason = null, CancellationToken cancellationToken = default)
    {
        State = TransportState.Closed;
        Closed?.Invoke(new TransportCloseInfo(code, reason));
        return Task.CompletedTask;
    }

    /// <summary>Simulate a server→client event from a raw JSON string.</summary>
    public void Emit(string json) => Message?.Invoke(json);

    /// <summary>The last action frame the client sent, parsed.</summary>
    public JsonElement LastSent()
        => JsonDocument.Parse(Sent[^1]).RootElement.Clone();

    public string LastRequestId() => LastSent().GetProperty("requestId").GetString()!;

    public void RaiseError(Exception ex) => Error?.Invoke(ex);
}
