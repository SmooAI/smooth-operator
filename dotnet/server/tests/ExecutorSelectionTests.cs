using Microsoft.Extensions.AI;
using Microsoft.Extensions.Logging;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;
using Core = SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Tests the env-gated executor selection seam (<see cref="ExecutorSelection"/>), the C# analog of
/// the Rust server's <c>runner.rs::turn_executor</c> tests. An injected durable executor wins; with
/// none supplied the selection defaults to <see cref="InProcessExecutor"/>, warning first if durable
/// mode was requested. The env is read through an injected delegate, so nothing here touches the
/// process environment.
/// </summary>
public class ExecutorSelectionTests
{
    /// <summary>A stand-in durable backend. Only its identity matters — the selection either returns
    /// THIS instance (injected wins) or an <see cref="InProcessExecutor"/> (it does not).</summary>
    // Fully qualified throughout: this test namespace nests under SmooAI.SmoothOperator (the protocol
    // client) and imports Microsoft.Extensions.AI, both of which shadow the core engine types the
    // interface uses — the bare names would bind the wrong types (CS0535).
    private sealed class FakeDurableExecutor : Core.IAgentExecutor
    {
        public Task<Core.AgentRunResponse> ExecuteAsync(Core.SmoothAgent agent, string message, Core.SmoothAgentThread? thread = null, CancellationToken cancellationToken = default) =>
            throw new NotImplementedException();

        public IAsyncEnumerable<ChatResponseUpdate> ExecuteStreamingAsync(Core.SmoothAgent agent, string message, Core.SmoothAgentThread? thread = null, CancellationToken cancellationToken = default) =>
            throw new NotImplementedException();
    }

    private static Func<string, string?> Env(string? durableValue) =>
        name => name == ExecutorSelection.DurableExecutorEnv ? durableValue : null;

    // ── injected executor wins (Rust: the `injected` branch of turn_executor) ─────────────────

    [Fact]
    public void InjectedExecutor_IsUsed_WhenSupplied()
    {
        var fake = new FakeDurableExecutor();
        var chosen = ExecutorSelection.TurnExecutor(fake, Env(null));
        Assert.Same(fake, chosen);
    }

    [Fact]
    public void InjectedExecutor_WinsOverEnv_AndEnvIsNotConsulted()
    {
        var fake = new FakeDurableExecutor();
        // Env asks for durable AND an executor is supplied → the supplied one is used, full stop.
        var chosen = ExecutorSelection.TurnExecutor(fake, Env("true"));
        Assert.Same(fake, chosen);
    }

    // ── no injection → in-process default (Rust: the fallback branch) ─────────────────────────

    [Fact]
    public void NoInjection_EnvOff_ResolvesInProcess()
    {
        var chosen = ExecutorSelection.TurnExecutor(null, Env(null));
        Assert.IsType<InProcessExecutor>(chosen);
    }

    [Fact]
    public void NoInjection_EnvOn_FallsBackToInProcess_AndWarns()
    {
        var logger = new CapturingLogger();
        var chosen = ExecutorSelection.TurnExecutor(null, Env("true"), logger);

        // Requested durable but nothing supplied: fall back rather than silently pretend durability…
        Assert.IsType<InProcessExecutor>(chosen);
        // …and say so loudly.
        Assert.Contains(logger.Warnings, w => w.Contains(ExecutorSelection.DurableExecutorEnv));
    }

    // ── the durable-request parse table (Rust: durable_requested) ─────────────────────────────

    [Theory]
    [InlineData("1", true)]
    [InlineData("true", true)]
    [InlineData("TRUE", true)]
    [InlineData(" on ", true)]
    [InlineData("yes", true)]
    [InlineData("0", false)]
    [InlineData("false", false)]
    [InlineData("off", false)]
    [InlineData("", false)]
    [InlineData(null, false)]
    public void DurableRequested_ParsesTruthiness(string? value, bool expected) =>
        Assert.Equal(expected, ExecutorSelection.DurableRequested(value));

    private sealed class CapturingLogger : ILogger
    {
        public List<string> Warnings { get; } = new();

        public IDisposable? BeginScope<TState>(TState state) where TState : notnull => null;

        public bool IsEnabled(LogLevel logLevel) => true;

        public void Log<TState>(LogLevel logLevel, EventId eventId, TState state, Exception? exception, Func<TState, Exception?, string> formatter)
        {
            if (logLevel == LogLevel.Warning)
            {
                Warnings.Add(formatter(state, exception));
            }
        }
    }
}
