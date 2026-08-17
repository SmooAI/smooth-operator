using Microsoft.Extensions.Logging;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Selects the <see cref="IAgentExecutor"/> a turn runs on — the one place the durable backend
/// (ADR-030) plugs in. The C# analog of the Rust server's <c>runner.rs::turn_executor</c>.
///
/// <para>An <b>injected</b> executor wins, full stop, and the env var is not consulted: that is the
/// seam a durable backend arrives through, and it is deliberately supplied from the DI container
/// rather than constructed here, so this server (and its published package) never takes a hard
/// dependency on <c>SmooAI.SmoothOperator.Temporal</c> or the <c>Temporalio</c> SDK. A host that
/// wants durable turns registers a <c>TemporalExecutor</c> (or any <see cref="IAgentExecutor"/>)
/// and it is used; a host that does not gets <see cref="InProcessExecutor"/> — a verbatim delegation
/// to <c>SmoothAgent.RunAsync</c>, so a deployment that never opts in behaves exactly as it did before
/// the seam existed.</para>
///
/// <para>Asking for durable mode via <see cref="DurableExecutorEnv"/> <i>without</i> supplying an
/// executor warns and falls back to in-process rather than silently pretending the turn is durable —
/// a turn a client believes will survive a disconnect, but won't, is worse than no durable mode at
/// all.</para>
/// </summary>
public static class ExecutorSelection
{
    /// <summary>Env var a deployment sets to run turns on a durable backend instead of in-process.
    /// Unset — the default — is the in-process executor. Byte-identical to the Rust server's
    /// <c>DURABLE_EXECUTOR_ENV</c>.</summary>
    public const string DurableExecutorEnv = "SMOOTH_AGENT_DURABLE_EXECUTOR";

    /// <summary>DI service key a host registers its durable <see cref="IAgentExecutor"/> under (e.g.
    /// <c>services.AddKeyedSingleton&lt;IAgentExecutor&gt;(ExecutorSelection.DurableExecutorServiceKey, temporalExecutor)</c>).
    /// Keyed so the durable registration never collides with the <i>effective</i> <see cref="IAgentExecutor"/>
    /// the server resolves via <see cref="TurnExecutor"/>.</summary>
    public const string DurableExecutorServiceKey = "durable";

    /// <summary>Whether the <see cref="DurableExecutorEnv"/> value asks for durable execution.
    /// Separated from the env read so the parse is testable without mutating process-global state —
    /// mirrors the Rust server's <c>durable_requested</c> (<c>1 | true | on | yes</c>, case-insensitive).</summary>
    public static bool DurableRequested(string? value) =>
        value?.Trim().ToLowerInvariant() is "1" or "true" or "on" or "yes";

    /// <summary>
    /// Resolve the executor a turn runs on. <paramref name="injected"/> (from DI) wins; otherwise a
    /// durable request via <paramref name="get"/>(<see cref="DurableExecutorEnv"/>) warns and falls
    /// back, and with nothing set this returns <see cref="InProcessExecutor"/>.
    /// </summary>
    /// <param name="injected">An optional durable executor supplied by the host (DI). Non-null ⇒ used.</param>
    /// <param name="get">Env reader (e.g. <see cref="Environment.GetEnvironmentVariable(string)"/>);
    /// injected so the selection is testable without touching the process environment.</param>
    /// <param name="logger">Optional logger for the "requested but unsupplied" warning.</param>
    public static IAgentExecutor TurnExecutor(IAgentExecutor? injected, Func<string, string?> get, ILogger? logger = null)
    {
        if (injected is not null)
        {
            return injected;
        }

        if (DurableRequested(get(DurableExecutorEnv)))
        {
            logger?.LogWarning(
                "durable execution requested via {Env} but no executor was supplied; running the turn in-process",
                DurableExecutorEnv);
        }

        return new InProcessExecutor();
    }
}
