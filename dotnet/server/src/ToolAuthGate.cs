using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Whether the requester's session is identity-verified (the C# analog of the monorepo's
/// <c>isSessionAuthenticated(conversationId)</c>). Consulted only for an <c>end_user</c>-level tool
/// on a <c>public</c> agent. The OTP/verification flow itself is the host's job — this seam is the
/// hook point. The default (<see cref="DenyAllSessionAuthenticator"/>) fails closed (returns false),
/// so an unwired server never runs an identity-gated tool for an unverified public visitor.
/// </summary>
public interface ISessionAuthenticator
{
    Task<bool> IsAuthenticatedAsync(string conversationId, CancellationToken cancellationToken = default);
}

/// <summary>Fail-closed default: no session is ever authenticated. A host swaps in a real one.</summary>
public sealed class DenyAllSessionAuthenticator : ISessionAuthenticator
{
    public static readonly DenyAllSessionAuthenticator Instance = new();

    public Task<bool> IsAuthenticatedAsync(string conversationId, CancellationToken cancellationToken = default) => Task.FromResult(false);
}

/// <summary>The gate's verdict for a tool call (before consulting the authenticator).</summary>
public enum ToolAuthOutcome
{
    /// <summary>Run the tool (no auth required, unsupported, or auto-satisfied on an internal agent).</summary>
    Allow,

    /// <summary>Admin-level tool on a public agent — never available; return the block message.</summary>
    BlockAdminOnPublic,

    /// <summary>Public agent, end_user level — run only if the session is authenticated.</summary>
    ConsultAuthenticator,
}

/// <summary>
/// authLevel enforcement + per-tool config delivery at TOOL-EXECUTION time — the C# analog of the
/// monorepo's <c>tool-execution.ts</c> auth gate and <c>registry.ts</c> config injection. Applied by
/// wrapping each gated tool in a <see cref="GatedTool"/> before the engine's agentic loop can call it;
/// a tool that isn't auth-gated and carries no config is passed through untouched (behavior unchanged).
/// </summary>
public static class ToolAuthGate
{
    /// <summary>The <see cref="AIFunctionArguments.Context"/> key under which a gated tool's per-tool
    /// <c>config</c> object is delivered to the tool at invocation (the analog of the reference's
    /// <c>ToolContext.toolSpecificConfig</c>). A host tool reads its config from here.</summary>
    public const string ToolConfigKey = "smooth.tool_config";

    /// <summary>Pure gate decision (mirrors the reference table). Only meaningful when the tool actually
    /// declares auth support; callers pass <paramref name="supportsAuth"/> from the tool registration.</summary>
    public static ToolAuthOutcome Evaluate(string authLevel, string visibility, bool supportsAuth)
    {
        if (authLevel == "none" || !supportsAuth)
        {
            return ToolAuthOutcome.Allow;
        }
        // Admin tools are internal-only; on a public agent they are never available.
        if (authLevel == "admin" && visibility == "public")
        {
            return ToolAuthOutcome.BlockAdminOnPublic;
        }
        // Internal agents: end_user (and admin) are auto-satisfied by the authenticated session.
        if (visibility == "internal")
        {
            return ToolAuthOutcome.Allow;
        }
        // Public agent, end_user level: needs an identity-verified session.
        return ToolAuthOutcome.ConsultAuthenticator;
    }

    public static string AdminBlockedMessage(string toolName) =>
        $"Tool '{toolName}' requires admin authentication and is not available on public-facing agents.";

    public static string VerificationRequiredMessage(string toolName) =>
        $"I need to verify your identity before I can use {toolName}. Please verify with a one-time code.";

    /// <summary>
    /// Wrap the turn's tools so auth-gated ones enforce their authLevel and config-bearing ones deliver
    /// their config at invocation. A no-op (returns <paramref name="tools"/> unchanged) when the agent
    /// has no <c>enabledTools</c> or none of them need gating/config — so the default path is untouched.
    /// </summary>
    public static IReadOnlyList<AITool> Apply(IReadOnlyList<AITool> tools, AgentConfig? config, ISessionAuthenticator authenticator, string conversationId)
    {
        if (config?.EnabledTools is not { Count: > 0 } entries || tools.Count == 0)
        {
            return tools;
        }
        if (!entries.Any(e => e.AuthLevel != "none" || e.Config is not null))
        {
            return tools;
        }

        var visibility = string.IsNullOrWhiteSpace(config.Visibility) ? "public" : config.Visibility!;
        var result = new List<AITool>(tools.Count);
        foreach (var tool in tools)
        {
            var entry = entries.FirstOrDefault(e => e.ToolId == tool.Name);
            var gate = entry is not null && (entry.AuthLevel != "none" || entry.Config is not null);
            if (gate && tool is AIFunction fn)
            {
                result.Add(new GatedTool(fn, entry!.AuthLevel, visibility, SupportsAuthRequirement(tool), entry.Config, authenticator, conversationId));
            }
            else
            {
                result.Add(tool);
            }
        }
        return result;
    }

    /// <summary>The tool's <c>supportsAuthRequirement</c> registration flag (default false) — a tool
    /// only participates in the authLevel gate when it opts in (faithful to the reference's
    /// <c>definition.supportsAuthRequirement</c>). Declared via <see cref="AITool.AdditionalProperties"/>.</summary>
    internal static bool SupportsAuthRequirement(AITool tool) =>
        tool.AdditionalProperties is { } props && props.TryGetValue("supportsAuthRequirement", out var value) && value is true;
}

/// <summary>
/// A tool decorator that enforces its authLevel and delivers its per-tool config, then delegates to the
/// wrapped tool. A blocked call returns the block message as the tool result (the model sees it and
/// responds), exactly like the reference pushing a <c>ToolMessage</c> and continuing.
/// </summary>
internal sealed class GatedTool : AIFunction
{
    private readonly AIFunction _inner;
    private readonly string _authLevel;
    private readonly string _visibility;
    private readonly bool _supportsAuth;
    private readonly JsonObject? _config;
    private readonly ISessionAuthenticator _authenticator;
    private readonly string _conversationId;

    public GatedTool(AIFunction inner, string authLevel, string visibility, bool supportsAuth, JsonObject? config, ISessionAuthenticator authenticator, string conversationId)
    {
        _inner = inner;
        _authLevel = authLevel;
        _visibility = visibility;
        _supportsAuth = supportsAuth;
        _config = config;
        _authenticator = authenticator;
        _conversationId = conversationId;
    }

    public override string Name => _inner.Name;

    public override string Description => _inner.Description;

    public override JsonElement JsonSchema => _inner.JsonSchema;

    public override IReadOnlyDictionary<string, object?> AdditionalProperties => _inner.AdditionalProperties;

    protected override async ValueTask<object?> InvokeCoreAsync(AIFunctionArguments arguments, CancellationToken cancellationToken)
    {
        switch (ToolAuthGate.Evaluate(_authLevel, _visibility, _supportsAuth))
        {
            case ToolAuthOutcome.BlockAdminOnPublic:
                return ToolAuthGate.AdminBlockedMessage(_inner.Name);
            case ToolAuthOutcome.ConsultAuthenticator:
                var authed = await _authenticator.IsAuthenticatedAsync(_conversationId, cancellationToken).ConfigureAwait(false);
                if (!authed)
                {
                    return ToolAuthGate.VerificationRequiredMessage(_inner.Name);
                }
                break;
        }

        // Allowed: deliver the per-tool config (if any) via the invocation context, then run the tool.
        if (_config is not null)
        {
            arguments.Context ??= new Dictionary<object, object?>();
            arguments.Context[ToolAuthGate.ToolConfigKey] = _config;
        }
        return await _inner.InvokeAsync(arguments, cancellationToken).ConfigureAwait(false);
    }
}
