using System.Net.WebSockets;
using System.Text;
using System.Text.Json.Nodes;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.TestHost;
using Microsoft.Extensions.AI;
using Microsoft.Extensions.DependencyInjection;
using SmooAI.SmoothOperator.Server.AspNetCore;

namespace SmooAI.SmoothOperator.Server.IntegrationTests;

/// <summary>
/// User-initiated turn cancellation — the <c>cancel</c> action ("Stop button"). The C# parity of the
/// Rust reference's <c>tests/turn_cancel.rs</c>, driven end-to-end over a REAL WebSocket against the
/// in-process ASP.NET Core host. Proves:
///
///   1. <b>Cancel mid-turn stops it.</b> A <c>cancel</c> frame while a turn is parked in a tool
///      cancels the turn's token — the in-flight await is abandoned (the tool's post-await line never
///      runs) — and a terminal <c>cancelled</c> event (status 499) is emitted. No
///      <c>eventual_response</c> follows.
///   2. <b>Cancel with no active turn is a silent no-op</b> (no event; connection stays live).
///   3. <b>A normal turn still completes</b> with an <c>eventual_response</c>.
///   4. <b>Disconnect mid-turn also aborts the turn</b> (no client remains to receive its output).
///   5. <b>One active turn per connection</b> — a second <c>send_message</c> mid-turn is rejected with
///      <c>TURN_IN_PROGRESS</c>, never run concurrently.
///
/// Runs fully offline: a scripted <see cref="MockChatClient"/> calls a deterministic tool that parks
/// the turn on a long delay, giving a stable in-flight window to cancel in.
/// </summary>
public class TurnCancelTests
{
    private const string SlowTool = "slow_probe";
    private static readonly TimeSpan Timeout = TimeSpan.FromSeconds(10);

    /// <summary>
    /// A tool that parks the turn: it signals that it started, then waits far longer than any test.
    /// A cancelled turn abandons that await — <see cref="Dropped"/> fires and <see cref="Finished"/>
    /// (the post-await line) never runs. The C# analog of the Rust test's drop-guard.
    /// </summary>
    private sealed class SlowToolProbe
    {
        public TaskCompletionSource Started { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource Dropped { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public volatile bool Finished;

        public AITool Tool => AIFunctionFactory.Create(
            async (CancellationToken ct) =>
            {
                Started.TrySetResult();
                try
                {
                    // Far longer than the test; only a cancellation ends this wait.
                    await Task.Delay(TimeSpan.FromSeconds(30), ct);
                }
                catch (OperationCanceledException)
                {
                    Dropped.TrySetResult();
                    throw;
                }
                // Only reached if the turn was NOT cancelled.
                Finished = true;
                return "done";
            },
            SlowTool,
            "parks the turn for cancellation tests");
    }

    private static WebApplication BuildApp(MockChatClient chat, AITool? tool = null)
    {
        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton<IChatClient>(chat);
        if (tool is not null)
        {
            builder.Services.AddSingleton<IReadOnlyList<AITool>>(new[] { tool });
        }
        builder.Services.AddSmoothOperatorServer();

        var app = builder.Build();
        app.MapSmoothOperatorWebSocket("/ws");
        return app;
    }

    /// <summary>A mock that calls the slow tool (so the turn parks in it and never returns on its own).</summary>
    private static MockChatClient SlowToolMock() =>
        new MockChatClient().PushToolCall("call-1", SlowTool, new Dictionary<string, object?>());

    private static async Task<WebSocket> ConnectAsync(TestServer server)
    {
        var client = server.CreateWebSocketClient();
        return await client.ConnectAsync(new Uri(server.BaseAddress, "ws"), CancellationToken.None);
    }

    private static Task SendAsync(WebSocket socket, JsonObject frame) =>
        socket.SendAsync(Encoding.UTF8.GetBytes(frame.ToJsonString()), WebSocketMessageType.Text, endOfMessage: true, CancellationToken.None);

    /// <summary>Read the next event verbatim (nothing skipped — a test asserting "the next event is a
    /// pong" needs the pong).</summary>
    private static async Task<JsonObject> NextEventAsync(WebSocket socket)
    {
        var buffer = new byte[16 * 1024];
        using var stream = new MemoryStream();
        WebSocketReceiveResult result;
        do
        {
            result = await socket.ReceiveAsync(buffer, CancellationToken.None);
            stream.Write(buffer, 0, result.Count);
        }
        while (!result.EndOfMessage);

        return JsonNode.Parse(Encoding.UTF8.GetString(stream.ToArray()))!.AsObject();
    }

    /// <summary>Read events until one of <paramref name="type"/> arrives, collecting the ones skipped.</summary>
    private static async Task<JsonObject> ReadUntilAsync(WebSocket socket, string type, List<JsonObject> seen)
    {
        while (true)
        {
            var ev = await NextEventAsync(socket).WaitAsync(Timeout);
            if (ev["type"]?.GetValue<string>() == type)
            {
                return ev;
            }
            seen.Add(ev);
        }
    }

    private static async Task<string> CreateSessionAsync(WebSocket socket)
    {
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "create_conversation_session",
            ["requestId"] = "r-create",
            ["agentId"] = "11111111-1111-1111-1111-111111111111",
        });
        var ev = await ReadUntilAsync(socket, "immediate_response", new List<JsonObject>());
        return ev["data"]!["sessionId"]!.GetValue<string>();
    }

    private static Task StartSlowTurnAsync(WebSocket socket, string sessionId, string requestId) =>
        SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = requestId,
            ["sessionId"] = sessionId,
            ["message"] = "please do the slow thing",
        });

    [Fact]
    public async Task CancelMidTurn_AbortsTheTurn_AndEmitsCancelled()
    {
        var probe = new SlowToolProbe();
        await using var app = BuildApp(SlowToolMock(), probe.Tool);
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket);

        await StartSlowTurnAsync(socket, sessionId, "turn-1");

        // Wait until the turn is genuinely in flight (parked in the tool's await).
        await probe.Started.Task.WaitAsync(Timeout);
        Assert.False(probe.Finished, "tool must not have finished yet");

        // Cancel it (reusing the turn's requestId, the correlation convention).
        await SendAsync(socket, new JsonObject { ["action"] = "cancel", ["requestId"] = "turn-1" });

        // A terminal `cancelled` event arrives, echoing the turn's requestId. (Skip any ack/stream
        // events that were in flight before the cancel landed.)
        var seen = new List<JsonObject>();
        var cancelled = await ReadUntilAsync(socket, "cancelled", seen);
        Assert.Equal("turn-1", cancelled["requestId"]!.GetValue<string>());
        Assert.Equal(499, cancelled["status"]!.GetValue<int>());
        Assert.Equal("turn-1", cancelled["data"]!["requestId"]!.GetValue<string>());
        Assert.Equal(499, cancelled["data"]!["status"]!.GetValue<int>());

        // The turn was dropped mid-await: the tool's wait was cancelled and its post-await line never ran.
        await probe.Dropped.Task.WaitAsync(Timeout);
        Assert.False(probe.Finished, "a cancelled turn's tool must never reach its post-await completion");

        // The connection is still alive and usable — and the pong is the NEXT event, so no
        // eventual_response was emitted for the cancelled turn.
        await SendAsync(socket, new JsonObject { ["action"] = "ping", ["requestId"] = "p1" });
        var pong = await NextEventAsync(socket).WaitAsync(Timeout);
        Assert.Equal("pong", pong["type"]!.GetValue<string>());
        Assert.Equal("p1", pong["requestId"]!.GetValue<string>());
        Assert.DoesNotContain(seen, e => e["type"]?.GetValue<string>() == "eventual_response");

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task CancelWithNoActiveTurn_IsASilentNoop()
    {
        await using var app = BuildApp(new MockChatClient().PushText("hi"));
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        _ = await CreateSessionAsync(socket);

        // Cancel with nothing running: must emit nothing.
        await SendAsync(socket, new JsonObject { ["action"] = "cancel", ["requestId"] = "nope" });

        // The next event is the pong (the cancel produced no event of its own).
        await SendAsync(socket, new JsonObject { ["action"] = "ping", ["requestId"] = "p1" });
        var ev = await NextEventAsync(socket).WaitAsync(Timeout);
        Assert.Equal("pong", ev["type"]!.GetValue<string>());
        Assert.Equal("p1", ev["requestId"]!.GetValue<string>());

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task NormalTurn_StillCompletes()
    {
        await using var app = BuildApp(new MockChatClient().PushText("All done here."));
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket);

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "turn-ok",
            ["sessionId"] = sessionId,
            ["message"] = "hello",
        });

        var seen = new List<JsonObject>();
        var done = await ReadUntilAsync(socket, "eventual_response", seen);
        Assert.Equal("turn-ok", done["requestId"]!.GetValue<string>());
        Assert.Equal(200, done["status"]!.GetValue<int>());
        // No cancellation happened.
        Assert.DoesNotContain(seen, e => e["type"]?.GetValue<string>() == "cancelled");

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task DisconnectMidTurn_AbortsTheTurn()
    {
        var probe = new SlowToolProbe();
        await using var app = BuildApp(SlowToolMock(), probe.Tool);
        await app.StartAsync();
        var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket);

        await StartSlowTurnAsync(socket, sessionId, "turn-x");
        await probe.Started.Task.WaitAsync(Timeout);

        // Client hangs up mid-turn. (A close frame, not an Abort: the in-memory TestServer transport
        // doesn't surface a client-side Abort to the server's ReceiveAsync — over a real socket that
        // path arrives as a WebSocketException, which the pump treats identically.)
        await socket.CloseOutputAsync(WebSocketCloseStatus.NormalClosure, "bye", CancellationToken.None);
        socket.Dispose();

        // The server aborts the in-flight turn: the tool's await is cancelled and its post-await
        // completion never runs.
        await probe.Dropped.Task.WaitAsync(Timeout);
        Assert.False(probe.Finished, "disconnect must abort the turn before it completes");

        await app.StopAsync();
    }

    [Fact]
    public async Task SecondSendMessageWhileTurnInFlight_IsRejected()
    {
        var probe = new SlowToolProbe();
        await using var app = BuildApp(SlowToolMock(), probe.Tool);
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket);

        await StartSlowTurnAsync(socket, sessionId, "turn-1");
        await probe.Started.Task.WaitAsync(Timeout);

        // A second send_message on the same connection while the first turn is parked.
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "turn-2",
            ["sessionId"] = sessionId,
            ["message"] = "and another thing",
        });

        var err = await ReadUntilAsync(socket, "error", new List<JsonObject>());
        Assert.Equal("TURN_IN_PROGRESS", err["error"]!["code"]!.GetValue<string>());
        Assert.Equal("turn-2", err["requestId"]!.GetValue<string>());
        // Rejected outright: the second turn never started (no second ack, no second tool run).
        Assert.False(probe.Finished);

        // Cancel the first turn so the connection tears down promptly.
        await SendAsync(socket, new JsonObject { ["action"] = "cancel", ["requestId"] = "turn-1" });
        var cancelled = await ReadUntilAsync(socket, "cancelled", new List<JsonObject>());
        Assert.Equal("turn-1", cancelled["requestId"]!.GetValue<string>());

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }
}
