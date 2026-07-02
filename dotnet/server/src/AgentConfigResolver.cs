using System.Collections.Concurrent;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Resolves an agent's per-agent configuration (<see cref="AgentConfig"/>) by agent id — the seam
/// through which per-agent <c>instructions</c> / <c>conversation_workflow</c> reach the server.
/// <c>create_conversation_session</c> carries only an agent UUID, so config is looked up server-side
/// per turn from the session's agent. This is the per-AGENT analog of the Rust server's per-ORG
/// persona settings seam (and mirrors the TS / Python lanes' <c>AgentConfigResolver</c>): a
/// multi-tenant host registers a resolver backed by the monorepo <c>agents</c> row (its
/// <c>instructions</c> + <c>conversation_workflow</c> jsonb, parsed tolerantly via
/// <see cref="AgentConfig.ParseInstructions"/> / <see cref="AgentConfig.ParseWorkflow"/>); absent
/// one, no per-agent config is applied and the server keeps its existing default-persona behavior
/// (byte-for-byte unchanged).
/// </summary>
public interface IAgentConfigResolver
{
    /// <summary>The config for <paramref name="agentId"/>, or <c>null</c> when there is none. MUST NOT
    /// throw for a malformed/unknown agent — return <c>null</c> so the session degrades to the
    /// default persona rather than failing.</summary>
    Task<AgentConfig?> ResolveAsync(string agentId, CancellationToken cancellationToken = default);
}

/// <summary>
/// A dict-backed <see cref="IAgentConfigResolver"/> — the reference resolver (and the analog of the
/// TS / Python <c>StaticAgentConfigResolver</c> and the Rust in-memory settings store). The default
/// (empty mapping) is the no-op resolver: every lookup returns <c>null</c> so behavior is unchanged.
/// Handy for tests and for a host that seeds config without a database.
/// </summary>
public sealed class StaticAgentConfigResolver : IAgentConfigResolver
{
    private readonly ConcurrentDictionary<string, AgentConfig> _configs = new();

    public StaticAgentConfigResolver Set(string agentId, AgentConfig config)
    {
        _configs[agentId] = config;
        return this;
    }

    public Task<AgentConfig?> ResolveAsync(string agentId, CancellationToken cancellationToken = default) =>
        Task.FromResult(_configs.TryGetValue(agentId, out var config) ? config : null);
}
