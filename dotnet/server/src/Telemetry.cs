using System.Diagnostics;
using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// OpenTelemetry GenAI instrumentation for the agent turn — the C# analog of the Rust server's
/// <c>smooth_operator::telemetry</c> module. <see cref="TurnRunner"/> opens a <see cref="SpanChat"/>
/// (<c>gen_ai.chat</c>) activity per turn carrying <see cref="GenAiSystem"/>,
/// <see cref="GenAiRequestModel"/>, <see cref="GenAiConversationId"/>, <see cref="GenAiAgentName"/>,
/// and — on completion — <see cref="GenAiUsageInputTokens"/> / <see cref="GenAiUsageOutputTokens"/>.
/// Each tool call opens a child <see cref="SpanTool"/> (<c>gen_ai.tool</c>) activity carrying
/// <see cref="GenAiToolName"/> and the (redacted) <see cref="GenAiToolArguments"/>.
/// <para>
/// The attribute keys are the canonical GenAI semantic-convention names, byte-identical to the Rust
/// telemetry module, so the observability studio groups Rust + .NET turns together. Registration is
/// env-gated in the host (an OTLP exporter only when <c>OTEL_EXPORTER_OTLP_ENDPOINT</c> is set),
/// mirroring the Rust <c>init_telemetry</c>; the <see cref="Source"/> emits nothing until a listener
/// (the OTel SDK, or a test's <see cref="ActivityListener"/>) starts sampling it.
/// </para>
/// </summary>
public static class Telemetry
{
    /// <summary>The <see cref="ActivitySource"/> name — hosts <c>AddSource</c> this to collect the spans.</summary>
    public const string ActivitySourceName = "smooth-operator";

    /// <summary>The value emitted for <see cref="GenAiSystem"/> — identifies these traces' origin.</summary>
    public const string SystemName = "smooth-operator";

    /// <summary>The agent name emitted as <see cref="GenAiAgentName"/> on the turn span.</summary>
    public const string AgentName = "smooth-agent-chat";

    // GenAI semantic-convention attribute keys (match rust/smooth-operator/src/telemetry.rs).
    public const string GenAiSystem = "gen_ai.system";
    public const string GenAiRequestModel = "gen_ai.request.model";
    public const string GenAiConversationId = "gen_ai.conversation.id";
    public const string GenAiUsageInputTokens = "gen_ai.usage.input_tokens";
    public const string GenAiUsageOutputTokens = "gen_ai.usage.output_tokens";
    public const string GenAiToolName = "gen_ai.tool.name";
    public const string GenAiToolArguments = "gen_ai.tool.call.arguments";
    public const string GenAiAgentName = "gen_ai.agent.name";

    /// <summary>
    /// <c>gen_ai.operation.name</c> — the operation a span represents.
    /// <para>
    /// The api-prime OTLP ingest takes this attribute VERBATIM when present and only derives it
    /// from the span name as a fallback, and its queries filter on <c>operation_name = 'tool'</c>.
    /// So the values must be exactly <see cref="OperationChat"/> / <see cref="OperationTool"/> —
    /// a spelling like <c>execute_tool</c> would land in the column and match nothing.
    /// </para></summary>
    public const string GenAiOperationName = "gen_ai.operation.name";

    /// <summary>
    /// <c>gen_ai.usage.cost_usd</c> — the turn's cost in USD.
    /// <para>
    /// Recorded ONLY when positive. A zero is ambiguous: the gateway answers 0 for a model it has
    /// no price for, and local pricing returns the free tier for anything it does not recognise, so
    /// a zero means "not measured", never "free". Exporting it would render a paid turn as a
    /// confident $0.00.
    /// </para></summary>
    public const string GenAiUsageCostUsd = "gen_ai.usage.cost_usd";

    /// <summary><c>smooai.gen_ai.cost_unavailable</c> — why <see cref="GenAiUsageCostUsd"/> is
    /// absent. Set INSTEAD of the cost, never alongside it. Same attribute name and values across
    /// every engine so a consumer never special-cases per language.</summary>
    public const string CostUnavailable = "smooai.gen_ai.cost_unavailable";

    /// <summary><see cref="CostUnavailable"/> value: no price could be established.</summary>
    public const string CostUnavailableUnpriced = "unpriced";

    /// <summary><c>smooai.org_id</c> — the owning org, matching every other engine.
    /// <para>ponytail: declared here but never set — the .NET server has no org concept at all
    /// (no <c>orgId</c> anywhere in <c>dotnet/server/src</c>), so wiring it is a plumbing change
    /// through FrameDispatcher, not a span tag. Set it here once that exists.</para></summary>
    public const string SmooaiOrgId = "smooai.org_id";

    /// <summary><see cref="GenAiOperationName"/> value on a <see cref="SpanChat"/> span.</summary>
    public const string OperationChat = "chat";

    /// <summary><see cref="GenAiOperationName"/> value on a <see cref="SpanTool"/> span.</summary>
    public const string OperationTool = "tool";

    /// <summary>Span name for the per-turn GenAI chat span (<c>gen_ai.chat</c>).</summary>
    public const string SpanChat = "gen_ai.chat";

    /// <summary>Span name for a per-tool-call child span (<c>gen_ai.tool</c>).</summary>
    public const string SpanTool = "gen_ai.tool";

    /// <summary>The process-global source every turn/tool span is started from.</summary>
    public static readonly ActivitySource Source = new(ActivitySourceName);

    /// <summary>Max length of a serialized tool-arguments string recorded on a span.</summary>
    private const int MaxToolArgsLen = 2048;

    private static readonly string[] SecretNeedles =
    {
        "secret", "token", "password", "api_key", "apikey",
        "authorization", "bearer", "credential", "access_key", "private_key",
    };

    /// <summary>
    /// Redact a tool's serialized JSON arguments for span recording — the C# port of the Rust
    /// <c>redact_tool_arguments</c>. Walks parsed JSON and replaces the value of any object key whose
    /// name looks secret-bearing (substring, case-insensitive) with <c>"[REDACTED]"</c>; non-JSON
    /// input passes through as-is. Always length-capped at <see cref="MaxToolArgsLen"/>.
    /// </summary>
    public static string RedactToolArguments(string arguments)
    {
        string redacted;
        try
        {
            var node = JsonNode.Parse(arguments);
            if (node is null)
            {
                redacted = arguments;
            }
            else
            {
                RedactInPlace(node);
                redacted = node.ToJsonString();
            }
        }
        catch (System.Text.Json.JsonException)
        {
            redacted = arguments; // not JSON — record the raw string, still capped below
        }
        return Truncate(redacted, MaxToolArgsLen);
    }

    private static bool IsSecretKey(string key)
    {
        var lower = key.ToLowerInvariant();
        return Array.Exists(SecretNeedles, n => lower.Contains(n, StringComparison.Ordinal));
    }

    private static void RedactInPlace(JsonNode node)
    {
        switch (node)
        {
            case JsonObject obj:
                // Snapshot keys — we mutate values while iterating.
                foreach (var key in obj.Select(kv => kv.Key).ToArray())
                {
                    if (IsSecretKey(key))
                    {
                        obj[key] = "[REDACTED]";
                    }
                    else if (obj[key] is { } child)
                    {
                        RedactInPlace(child);
                    }
                }
                break;
            case JsonArray arr:
                foreach (var item in arr)
                {
                    if (item is not null)
                    {
                        RedactInPlace(item);
                    }
                }
                break;
        }
    }

    private static string Truncate(string s, int max) =>
        s.Length <= max ? s : string.Concat(s.AsSpan(0, max), "…");
}
