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
        foreach (var entry in entries)
        {
            var name = (entry?["model_name"] as JsonValue)?.GetValue<string>();
            if (string.IsNullOrEmpty(name))
            {
                continue;
            }
            var ceiling = TryGetPositiveInt(entry?["model_info"]?["max_output_tokens"]);
            if (ceiling is not null)
            {
                map[name] = ceiling.Value;
            }
        }
        return map;
    }

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
