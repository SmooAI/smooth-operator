using Microsoft.Extensions.Logging;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// A turn must DEGRADE — not die — when knowledge retrieval fails (e.g. the embedding gateway is
/// down). Before pearl th-dadde3 a throwing <c>QueryAsync</c> propagated out of <see cref="TurnRunner"/>
/// and the dispatcher surfaced INTERNAL_ERROR, killing the whole turn. Now the failure is caught, the
/// turn proceeds with empty grounding, and a warning is logged.
/// </summary>
public class RetrievalDegradationTests
{
    [Fact]
    public async Task RetrievalFailure_DegradesTurn_NoCitations_AndWarns()
    {
        var chat = new MockChatClient().PushText("Here's what I can tell you.");
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var logger = new CapturingLogger();
        var runner = new TurnRunner(chat, store, knowledge: new ThrowingKnowledgeBase(), logger: logger);

        // Regression: without the fix this throws (the gateway-down QueryAsync propagates), which the
        // dispatcher turns into INTERNAL_ERROR. With the fix the turn completes normally.
        var result = await runner.RunAsync(session.ConversationId, "r1", "How long is the return window?", _ => { });

        Assert.Equal("Here's what I can tell you.", result.Reply);
        Assert.Empty(result.Citations); // grounding dropped, not fatal
        Assert.Contains(logger.Entries, e => e.Level == LogLevel.Warning);
    }

    [Fact]
    public async Task RetrievalFailure_WithoutLogger_StillDegradesGracefully()
    {
        var chat = new MockChatClient().PushText("ok");
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var runner = new TurnRunner(chat, store, knowledge: new ThrowingKnowledgeBase()); // no logger wired

        var result = await runner.RunAsync(session.ConversationId, "r1", "hi", _ => { });

        Assert.Equal("ok", result.Reply);
        Assert.Empty(result.Citations);
    }

    /// <summary>A knowledge base whose retrieval always fails — the embedding-gateway-down case.</summary>
    private sealed class ThrowingKnowledgeBase : IKnowledgeBase
    {
        public Task IngestAsync(KnowledgeDocument document, CancellationToken cancellationToken = default) => Task.CompletedTask;

        public Task<IReadOnlyList<KnowledgeResult>> QueryAsync(string query, int topK, CancellationToken cancellationToken = default) =>
            throw new InvalidOperationException("embedding gateway unavailable");
    }

    private sealed class CapturingLogger : ILogger
    {
        public List<(LogLevel Level, string Message)> Entries { get; } = new();

        public IDisposable? BeginScope<TState>(TState state) where TState : notnull => null;

        public bool IsEnabled(LogLevel logLevel) => true;

        public void Log<TState>(LogLevel logLevel, EventId eventId, TState state, Exception? exception, Func<TState, Exception?, string> formatter) =>
            Entries.Add((logLevel, formatter(state, exception)));
    }
}
