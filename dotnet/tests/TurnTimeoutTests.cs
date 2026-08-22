// A .NET turn must be bounded, like every other client's.
//
// Before this, SmoothAgentClientOptions exposed only RequestTimeout — nothing
// matched TypeScript's turnTimeout (120s), Go's DefaultTurnTimeout (120s) or
// Python's turn_timeout (120.0). A turn the server accepted but never terminated
// hung for the life of the process, with no error, no diagnostic, and a leaked
// entry in the client's turn table. Both tests here bound their wait rather than
// awaiting outright: without the fix the awaited task never settles at all, and a
// test that merely hangs never reports a failure.

namespace SmooAI.SmoothOperator.Tests;

public sealed class TurnTimeoutTests
{
    /// <summary>Generous relative to the 100ms turn timeout under test, but finite — this is
    /// what turns "hangs forever" into a reported failure.</summary>
    private static readonly TimeSpan Bound = TimeSpan.FromSeconds(10);

    private static async Task<T> WithinBound<T>(Task<T> task, string what)
    {
        var winner = await Task.WhenAny(task, Task.Delay(Bound)).ConfigureAwait(false);
        Assert.True(winner == task, $"{what} never settled within {Bound.TotalSeconds}s — it hung.");
        return await task.ConfigureAwait(false);
    }

    private static (SmoothAgentClient Client, MockTransport Transport) MakeClient(TimeSpan turnTimeout)
    {
        var transport = new MockTransport();
        var counter = 0;
        var client = new SmoothAgentClient(new SmoothAgentClientOptions
        {
            Url = "wss://test",
            Transport = transport,
            GenerateRequestId = () => $"req-test-{++counter}",
            RequestTimeout = TimeSpan.FromSeconds(1),
            TurnTimeout = turnTimeout,
        });
        return (client, transport);
    }

    /// <summary>
    /// The server accepts the message and then goes silent — no eventual_response, no
    /// error, no cancelled. The turn must fault with a TurnTimeoutException rather than
    /// park forever.
    /// </summary>
    [Fact]
    public async Task Turn_TimesOut_WhenServerNeverSendsATerminalEvent()
    {
        var (client, transport) = MakeClient(TimeSpan.FromMilliseconds(100));
        await client.ConnectAsync();

        var turn = client.SendMessageAsync(new SendMessageAction { SessionId = "sess-1", Message = "hi", Stream = true });
        var reqId = transport.LastRequestId();

        // A partial stream is not a terminal event: activity must not settle the turn.
        transport.Emit("""{"type":"stream_token","requestId":"RID","token":"Hel","data":{"requestId":"RID","token":"Hel"}}""".Replace("RID", reqId));

        var completion = Assert.ThrowsAsync<TurnTimeoutException>(() => turn.Completion);
        var ex = await WithinBound(completion, "turn.Completion");
        Assert.Equal(reqId, ex.RequestId);
    }

    /// <summary>
    /// The async iterator must surface the timeout too — a caller that only does
    /// `await foreach` and never awaits Completion is exactly the caller that used to
    /// block forever.
    /// </summary>
    [Fact]
    public async Task TurnIteration_ThrowsTurnTimeout_WhenServerGoesSilent()
    {
        var (client, transport) = MakeClient(TimeSpan.FromMilliseconds(100));
        await client.ConnectAsync();

        var turn = client.SendMessageAsync(new SendMessageAction { SessionId = "sess-1", Message = "hi", Stream = true });

        var iterate = Task.Run(async () =>
        {
            var seen = 0;
            await foreach (var _ in turn) seen++;
            return seen;
        });

        var ex = await WithinBound(
            Assert.ThrowsAsync<TurnTimeoutException>(() => iterate),
            "await foreach over the turn");
        Assert.Equal(turn.RequestId, ex.RequestId);
    }

    /// <summary>
    /// The turn frame is sent fire-and-forget. SendAsync is async, so a send failure
    /// FAULTS the returned task rather than throwing at the call site — a try/catch
    /// wrapped around a discarded `_ = SendAsync(...)` never runs, so the turn was never
    /// aborted and leaked in the client's turn table with no error. The task has to be
    /// observed for the abort to happen.
    /// </summary>
    [Fact]
    public async Task Turn_Aborts_WhenTheSendFailsAsynchronously()
    {
        // Turn timeout disabled, so nothing but the send-failure path can settle this turn.
        var (client, transport) = MakeClient(Timeout.InfiniteTimeSpan);
        await client.ConnectAsync();
        var boom = new IOException("socket went away mid-send");
        transport.AsyncSendFailure = boom;

        var turn = client.SendMessageAsync(new SendMessageAction { SessionId = "sess-1", Message = "hi", Stream = true });

        var ex = await WithinBound(
            Assert.ThrowsAsync<IOException>(() => turn.Completion),
            "turn.Completion after an async send failure");
        Assert.Same(boom, ex);
    }
}
