using System.Text.Json;
using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// A single step in a structured conversation workflow — the C# analog of the monorepo's
/// <c>ConversationWorkflowStep</c> (packages/schemas/src/agents/agent.ts). The judge evaluates
/// <see cref="Criteria"/> after each turn to decide whether to advance to <see cref="Next"/> (or,
/// absent that, the next step in order).
/// </summary>
public sealed record ConversationWorkflowStep(string Id, string Intent, string Criteria, string? Next);

/// <summary>
/// A structured conversation workflow: an overall <see cref="Goal"/> plus ordered
/// <see cref="Steps"/>. When an agent has one, the current step's intent + criteria are rendered
/// into the system prompt and a post-turn judge advances the pointer. The C# analog of the
/// monorepo's <c>ConversationWorkflow</c>.
/// </summary>
public sealed record ConversationWorkflow(string Goal, IReadOnlyList<ConversationWorkflowStep> Steps);

/// <summary>
/// One entry of the agent's <c>tool_config.enabledTools</c> (mirrors the monorepo
/// <c>AgentToolConfig</c>). <see cref="ToolId"/> is snake_case (matches the runtime tool registry
/// keys). <see cref="AuthLevel"/> / <see cref="Config"/> are preserved for the host even though the
/// reference server doesn't act on them yet.
/// </summary>
public sealed record EnabledTool(string ToolId, bool Enabled, string AuthLevel, JsonObject? Config);

/// <summary>
/// Per-agent configuration resolved for a conversation (the analog of the monorepo <c>agents</c>
/// row's <c>instructions</c> / <c>conversation_workflow</c> / <c>greeting</c> / <c>personality</c>).
/// The server applies this ON TOP of the org/host default persona so each agent behaves as itself
/// rather than a generic customer-support persona.
///
/// Every field is optional: an agent with no config (or malformed config — see the tolerant
/// <c>Parse*</c> helpers) resolves to a fully-null instance, and the server falls back to its
/// existing default-persona behavior. Never throws on bad input — a broken jsonb blob degrades a
/// session to the default, it never crashes it.
/// </summary>
public sealed record AgentConfig(
    string? InstructionsPrompt = null,
    ConversationWorkflow? Workflow = null,
    string? Greeting = null,
    string? Personality = null,
    IReadOnlyList<EnabledTool>? EnabledTools = null,
    string? Visibility = null)
{
    /// <summary>An empty config — the "no per-agent overrides" sentinel.</summary>
    public static readonly AgentConfig Empty = new();

    /// <summary>True when this config carries nothing the server would apply. A non-null
    /// <see cref="EnabledTools"/> (even empty) is a tool restriction, so it counts as configured.</summary>
    public bool IsEmpty => string.IsNullOrWhiteSpace(InstructionsPrompt) && Workflow is null && string.IsNullOrWhiteSpace(Greeting) && string.IsNullOrWhiteSpace(Personality) && EnabledTools is null;

    /// <summary>
    /// Parse the <c>instructions</c> jsonb (<c>{"prompt": "..."}</c>) into the freeform prompt
    /// string. Tolerant: null/blank/malformed/absent-<c>prompt</c> → <c>null</c> (no override).
    /// A bare JSON string is also accepted (treated as the prompt) for forgiveness.
    /// </summary>
    public static string? ParseInstructions(string? json)
    {
        if (string.IsNullOrWhiteSpace(json))
        {
            return null;
        }
        try
        {
            var node = JsonNode.Parse(json);
            var prompt = node switch
            {
                JsonObject obj => obj["prompt"]?.GetValue<string>(),
                JsonValue val when val.TryGetValue<string>(out var s) => s,
                _ => null,
            };
            return string.IsNullOrWhiteSpace(prompt) ? null : prompt;
        }
        catch (Exception ex) when (ex is JsonException or FormatException or InvalidOperationException)
        {
            return null;
        }
    }

    /// <summary>
    /// Parse the <c>conversation_workflow</c> jsonb into a <see cref="ConversationWorkflow"/>.
    /// Tolerant of every malformed shape (null/blank/not-an-object/missing-goal/empty-steps/bad-step)
    /// → <c>null</c>, so a broken workflow silently degrades to freeform-prompt behavior rather than
    /// crashing the session. Enforces the schema bounds (1..20 steps; non-empty id/intent/criteria)
    /// so a partially-valid blob doesn't render a garbage prompt section.
    /// </summary>
    public static ConversationWorkflow? ParseWorkflow(string? json)
    {
        if (string.IsNullOrWhiteSpace(json))
        {
            return null;
        }
        try
        {
            if (JsonNode.Parse(json) is not JsonObject obj)
            {
                return null;
            }

            var goal = obj["goal"]?.GetValue<string>();
            if (string.IsNullOrWhiteSpace(goal))
            {
                return null;
            }

            if (obj["steps"] is not JsonArray stepsArray || stepsArray.Count is 0 or > 20)
            {
                return null;
            }

            var steps = new List<ConversationWorkflowStep>(stepsArray.Count);
            foreach (var element in stepsArray)
            {
                if (element is not JsonObject stepObj)
                {
                    return null;
                }
                var id = stepObj["id"]?.GetValue<string>();
                var intent = stepObj["intent"]?.GetValue<string>();
                var criteria = stepObj["criteria"]?.GetValue<string>();
                if (string.IsNullOrWhiteSpace(id) || string.IsNullOrWhiteSpace(intent) || string.IsNullOrWhiteSpace(criteria))
                {
                    return null;
                }
                var next = stepObj["next"]?.GetValue<string>();
                steps.Add(new ConversationWorkflowStep(id, intent, criteria, string.IsNullOrWhiteSpace(next) ? null : next));
            }

            return new ConversationWorkflow(goal, steps);
        }
        catch (Exception ex) when (ex is JsonException or FormatException or InvalidOperationException)
        {
            return null;
        }
    }

    /// <summary>
    /// Parse the agent's <c>tool_config</c> jsonb (<c>{ enabledTools: [{ toolId, enabled?, authLevel?,
    /// config? }] }</c>, per the monorepo <c>AgentToolConfig</c>) into the enabled-tool list used to
    /// restrict the server's tool set. Returns <c>null</c> = NO restriction (full tool set) when the
    /// config is missing/blank/malformed OR <c>enabledTools</c> is empty — empty must NOT mean
    /// "no tools" (every agent row defaults <c>enabledTools</c> to <c>[]</c>). A non-empty
    /// <c>enabledTools</c> yields a non-null list (even if all entries were dropped as malformed),
    /// so the restriction stays active and fails closed to least-privilege. Individual entries with a
    /// missing / non-snake_case <c>toolId</c> are dropped (tolerant); unknown toolIds are ignored at
    /// filter time by intersecting with the registered tools.
    /// </summary>
    public static IReadOnlyList<EnabledTool>? ParseToolConfig(string? json)
    {
        if (string.IsNullOrWhiteSpace(json))
        {
            return null;
        }
        try
        {
            if (JsonNode.Parse(json) is not JsonObject obj || obj["enabledTools"] is not JsonArray entries || entries.Count == 0)
            {
                return null;
            }

            var tools = new List<EnabledTool>(entries.Count);
            foreach (var element in entries)
            {
                if (element is not JsonObject entry)
                {
                    continue;
                }
                var toolId = entry["toolId"]?.GetValue<string>();
                // snake_case only — camelCase ids silently fail to bind at runtime (SMOODEV-981).
                if (string.IsNullOrWhiteSpace(toolId) || !IsSnakeCase(toolId))
                {
                    continue;
                }
                var enabled = entry["enabled"] is JsonValue e && e.TryGetValue<bool>(out var b) ? b : true;
                var authLevel = entry["authLevel"]?.GetValue<string>() ?? "none";
                tools.Add(new EnabledTool(toolId, enabled, string.IsNullOrWhiteSpace(authLevel) ? "none" : authLevel, entry["config"] as JsonObject));
            }
            // Non-empty enabledTools → keep the restriction active even if every entry was dropped.
            return tools;
        }
        catch (Exception ex) when (ex is JsonException or FormatException or InvalidOperationException)
        {
            return null;
        }
    }

    private static bool IsSnakeCase(string value)
    {
        foreach (var ch in value)
        {
            if (!(ch is >= 'a' and <= 'z' or >= '0' and <= '9' or '_'))
            {
                return false;
            }
        }
        return true;
    }
}
