using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Reads a model's HARD output ceiling (<c>max_output_tokens</c>) from the LiteLLM gateway's
/// <c>/model/info</c>, so the chat path can clamp <c>max_tokens</c> to what the model can physically
/// emit (EPIC th-1cc9fa). The C# analog of the Rust server's <c>admin::model_output_ceiling</c> +
/// <c>map_model_info</c> — kept out of the engine so the published engine takes no LiteLLM-specific
/// HTTP dependency. Best-effort: any gateway error, an unknown model, or a model with no positive
/// ceiling ⇒ <c>null</c> ⇒ the engine leaves <c>max_tokens</c> unclamped (graceful, no behavior change).
/// </summary>
public static class ModelInfo
{
    /// <summary>
    /// Parse the gateway's <c>/model/info</c> payload
    /// (<c>{ data: [{ model_name, model_info: { max_output_tokens } }] }</c>) into a
    /// <c>model_name → ceiling</c> map. Entries without a <c>model_name</c> or with a missing /
    /// non-positive ceiling are dropped. Pure + network-free, so it's unit-testable on a sample payload.
    /// </summary>
    public static IReadOnlyDictionary<string, int> ParseCeilings(JsonNode? payload)
    {
        var map = new Dictionary<string, int>(StringComparer.Ordinal);
        if (payload?["data"] is not JsonArray entries)
        {
            return map;
        }
        foreach (var raw in entries)
        {
            // Coerced, not indexed blind: indexing a non-object JsonNode throws, so a payload with a
            // scalar `model_info` (or a scalar entry) would otherwise take the whole parse down.
            var entry = raw as JsonObject;
            var name = Name(entry);
            if (name is null)
            {
                continue;
            }
            var ceiling = TryGetPositiveInt((entry?["model_info"] as JsonObject)?["max_output_tokens"]);
            if (ceiling is not null)
            {
                map[name] = ceiling.Value;
            }
        }
        return map;
    }

    /// <summary>
    /// Map the gateway's <c>/model/info</c> payload into the shape
    /// <c>GET /admin/model-costs</c> answers — <c>{ "&lt;model&gt;": { inputCostPerToken,
    /// outputCostPerToken, tier, useCases, maxOutputTokens } }</c> — matching the Rust
    /// <c>map_model_info</c> and its Go/TS/Python ports. Pure + network-free, so it is unit-testable
    /// on a sample payload.
    /// <para>
    /// Entries without a <c>model_name</c> are skipped, and every field is <b>null when the gateway
    /// omits it</b> rather than defaulted: a <c>0</c> cost would render a free-model badge on a paid
    /// model, and a defaulted ceiling would clamp a model that has none.
    /// </para>
    /// <para>
    /// Deliberately separate from <see cref="ParseCeilings"/>, which is a different contract: it drops
    /// non-positive ceilings because a bogus clamp is worse than none, whereas this reports whatever
    /// the gateway said (including null) because the console is displaying it, not clamping on it.
    /// </para>
    /// </summary>
    public static JsonObject MapModelInfo(JsonNode? payload)
    {
        var out_ = new JsonObject();
        if (payload?["data"] is not JsonArray entries)
        {
            return out_;
        }
        foreach (var raw in entries)
        {
            // Every level is coerced rather than indexed blind: indexing a JsonNode that is not an
            // object THROWS, so a gateway payload with e.g. `model_info: 7` would take down the read.
            var name = Name(raw as JsonObject);
            if (name is null || out_.ContainsKey(name))
            {
                continue;
            }
            var info = (raw as JsonObject)?["model_info"] as JsonObject;
            out_[name] = new JsonObject
            {
                ["inputCostPerToken"] = Num(info, "input_cost_per_token"),
                ["outputCostPerToken"] = Num(info, "output_cost_per_token"),
                ["tier"] = Str(info, "model_tier") is { } tier ? JsonValue.Create(tier) : null,
                ["useCases"] = info?["use_cases"] is JsonArray cases ? cases.DeepClone() : new JsonArray(),
                ["maxOutputTokens"] = Num(info, "max_output_tokens"),
            };
        }
        return out_;
    }

    /// <summary>The entry's non-empty <c>model_name</c>, or <c>null</c> when absent or not a string.</summary>
    private static string? Name(JsonObject? entry) => Str(entry, "model_name") is { Length: > 0 } name ? name : null;

    /// <summary>The named field as a JSON string, or <c>null</c> when absent or not a string.</summary>
    private static string? Str(JsonObject? obj, string key) =>
        obj?[key] is JsonValue value && value.TryGetValue<string>(out var s) ? s : null;

    /// <summary>The named field as a JSON number, or <c>null</c> when absent or non-numeric.</summary>
    private static JsonNode? Num(JsonObject? info, string key) =>
        info?[key] is JsonValue value && value.TryGetValue<double>(out var d) ? JsonValue.Create(d) : null;

    /// <summary>
    /// Fetch the output ceiling for <paramref name="model"/> from <c>{gateway}/model/info</c> via
    /// <paramref name="http"/> (its <see cref="HttpClient.BaseAddress"/> is the gateway root — with a
    /// trailing slash — and the auth header is already set). Returns <c>null</c> on ANY error, an
    /// unknown model, or a model with no positive ceiling; the caller then leaves <c>max_tokens</c>
    /// unclamped. Never throws.
    /// </summary>
    public static async Task<int?> FetchCeilingAsync(HttpClient http, string model, CancellationToken cancellationToken = default)
    {
        try
        {
            using var response = await http.GetAsync("model/info", cancellationToken).ConfigureAwait(false);
            if (!response.IsSuccessStatusCode)
            {
                return null;
            }
            var body = await response.Content.ReadAsStringAsync(cancellationToken).ConfigureAwait(false);
            var ceilings = ParseCeilings(JsonNode.Parse(body));
            return ceilings.TryGetValue(model, out var ceiling) ? ceiling : null;
        }
        catch
        {
            // Best-effort: a gateway blip must never fail a boot or a turn — just skip the clamp.
            return null;
        }
    }

    /// <summary>The node as a positive <see cref="int"/> (accepting int/long/double JSON numbers), or
    /// <c>null</c> when it's absent, non-numeric, or ≤ 0 (a bogus ceiling must not clamp to nothing).</summary>
    private static int? TryGetPositiveInt(JsonNode? node)
    {
        if (node is not JsonValue value)
        {
            return null;
        }
        long? number = value.TryGetValue<long>(out var l) ? l
            : value.TryGetValue<double>(out var d) && d is >= long.MinValue and <= long.MaxValue ? (long)d
            : null;
        return number is > 0 and <= int.MaxValue ? (int)number.Value : null;
    }
}
