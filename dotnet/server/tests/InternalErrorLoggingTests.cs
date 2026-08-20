using System.Runtime.CompilerServices;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using Microsoft.Extensions.Logging;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// An <c>INTERNAL_ERROR</c> on the wire must leave a stack behind in the host log (th-e7ef23).
/// Both catch sites in <see cref="FrameDispatcher"/> used to discard the exception, so a server whose
/// every turn failed logged NOTHING at all — the failure that hid th-df7007 (the .NET host read the
/// wrong gateway env var, 401'd on every turn, and reported only "Internal error processing the
/// request."). The wire message stays generic; the detail goes to the log.
/// </summary>
public class InternalErrorLoggingTests
{
    [Fact]
    public async Task TurnFailure_EmitsInternalError_AndLogsTheExceptionWithStack()
    {
        var logger = new CapturingLogger();
        var store = new InMemorySessionStore();
        var dispatcher = new FrameDispatcher(store, new ThrowingChatClient("gateway said 401"), logger: logger);
        var events = new List<JsonObject>();

        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","agentId":"11111111-1111-1111-1111-111111111111","requestId":"r1"}""", events.Add);
        var sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
        await dispatcher.DispatchAsync(
            $$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"hi"}""",
            events.Add);
        await dispatcher.WaitForTurnsAsync();

        // Wire contract unchanged: generic code + message, no exception detail leaked.
        var terminal = events[^1];
        Assert.Equal("error", terminal["type"]!.GetValue<string>());
        Assert.Equal("INTERNAL_ERROR", terminal["error"]!["code"]!.GetValue<string>());
        // Drop the epoch-millis timestamp before the leak check: it is server noise, and
        // whenever it happened to contain the digits "401" this assertion flaked in CI.
        terminal["timestamp"] = 0;
        Assert.DoesNotContain("401", terminal.ToJsonString());

        // …but the host log has the real cause, at Error level, with the exception attached.
        var logged = Assert.Single(logger.Entries, e => e.Level == LogLevel.Error);
        Assert.Contains("send_message", logged.Message);
        Assert.Contains("r2", logged.Message);
        Assert.Contains("gateway said 401", logged.Message);
        Assert.NotNull(logged.Exception);
    }

    [Fact]
    public async Task DispatcherFailure_EmitsInternalError_AndLogsTheException()
    {
        var logger = new CapturingLogger();
        var dispatcher = BuildDispatcherWithThrowingResolver(logger);
        var events = new List<JsonObject>();

        await DriveFailingSendAsync(dispatcher, events);

        Assert.Equal("INTERNAL_ERROR", events[^1]["error"]!["code"]!.GetValue<string>());
        var logged = Assert.Single(logger.Entries, e => e.Level == LogLevel.Error);
        Assert.Contains("send_message", logged.Message);
        Assert.NotNull(logged.Exception);
    }

    [Fact]
    public async Task NoLogger_StillEmitsInternalError()
    {
        var dispatcher = BuildDispatcherWithThrowingResolver(logger: null); // no logger wired
        var events = new List<JsonObject>();

        await DriveFailingSendAsync(dispatcher, events);

        Assert.Equal("INTERNAL_ERROR", events[^1]["error"]!["code"]!.GetValue<string>());
    }

    /// <summary>A dispatcher whose per-agent config lookup throws — a handler bug that surfaces
    /// SYNCHRONOUSLY inside <c>DispatchAsync</c>'s guard (before the turn is spawned), which is the path
    /// under test. It stands in for the old trigger, a non-string <c>conversationId</c>: type-mismatched
    /// frame fields are now a clean <c>VALIDATION_ERROR</c> rather than an exception (th-acf8ea), and a
    /// test that keeps asserting INTERNAL_ERROR on one would be asserting the defect.</summary>
    private static FrameDispatcher BuildDispatcherWithThrowingResolver(ILogger? logger) =>
        new(new InMemorySessionStore(), new MockChatClient(), agentConfigResolver: new ThrowingAgentConfigResolver(), logger: logger);

    private static async Task DriveFailingSendAsync(FrameDispatcher dispatcher, List<JsonObject> events)
    {
        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","agentId":"11111111-1111-1111-1111-111111111111","requestId":"r1"}""", events.Add);
        var sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
        await dispatcher.DispatchAsync($$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"hi"}""", events.Add);
        await dispatcher.WaitForTurnsAsync();
    }

    private sealed class ThrowingAgentConfigResolver : IAgentConfigResolver
    {
        public Task<AgentConfig?> ResolveAsync(string agentId, CancellationToken cancellationToken = default) =>
            throw new InvalidOperationException("agent config store exploded");
    }

    private sealed class ThrowingChatClient(string message) : IChatClient
    {
        public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default) =>
            throw new InvalidOperationException(message);

        public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
            IEnumerable<ChatMessage> messages,
            ChatOptions? options = null,
            [EnumeratorCancellation] CancellationToken cancellationToken = default)
        {
            await Task.Yield();
            throw new InvalidOperationException(message);
#pragma warning disable CS0162 // unreachable — required to make this an iterator
            yield break;
#pragma warning restore CS0162
        }

        public object? GetService(Type serviceType, object? serviceKey = null) => null;

        public void Dispose()
        {
        }
    }

    private sealed class CapturingLogger : ILogger
    {
        public List<(LogLevel Level, string Message, Exception? Exception)> Entries { get; } = new();

        public IDisposable? BeginScope<TState>(TState state) where TState : notnull => null;

        public bool IsEnabled(LogLevel logLevel) => true;

        public void Log<TState>(LogLevel logLevel, EventId eventId, TState state, Exception? exception, Func<TState, Exception?, string> formatter) =>
            Entries.Add((logLevel, formatter(state, exception), exception));
    }
}
