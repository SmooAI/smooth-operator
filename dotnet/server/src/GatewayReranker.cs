using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// A production <see cref="IReranker"/> backed by a Cohere/Voyage-style cross-encoder
/// <c>/rerank</c> endpoint over the gateway. POSTs <c>{ model, query, documents, top_n }</c> and
/// reorders the candidates by the returned <c>index → relevance_score</c>. The C# analog of the
/// Rust adapter's <c>GatewayReranker</c>. Like <see cref="GatewayEmbedder"/>, the caller supplies a
/// configured <see cref="HttpClient"/> (BaseAddress = the gateway, with the auth header), and the
/// HTTP path is unit-tested against a fake handler so it runs in CI without a gateway. Defensive:
/// if the endpoint returns nothing usable, the upstream order is preserved (truncated to top-K).
/// </summary>
public sealed class GatewayReranker : IReranker
{
    private static readonly JsonSerializerOptions JsonOptions = new() { PropertyNameCaseInsensitive = true };

    private readonly HttpClient _http;
    private readonly string _model;

    public GatewayReranker(HttpClient httpClient, string model)
    {
        _http = httpClient ?? throw new ArgumentNullException(nameof(httpClient));
        _model = model;
    }

    public async Task<IReadOnlyList<KnowledgeResult>> RerankAsync(string query, IReadOnlyList<KnowledgeResult> candidates, int topK, CancellationToken cancellationToken = default)
    {
        if (candidates.Count == 0 || topK <= 0)
        {
            return Array.Empty<KnowledgeResult>();
        }

        var documents = candidates.Select(c => c.Chunk).ToArray();
        var payload = JsonSerializer.Serialize(new { model = _model, query, documents, top_n = topK });
        using var content = new StringContent(payload, Encoding.UTF8, "application/json");
        using var response = await _http.PostAsync("rerank", content, cancellationToken).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();

        await using var stream = await response.Content.ReadAsStreamAsync(cancellationToken).ConfigureAwait(false);
        var parsed = await JsonSerializer.DeserializeAsync<RerankResponse>(stream, JsonOptions, cancellationToken).ConfigureAwait(false);

        var results = parsed?.Results;
        if (results is null || results.Count == 0)
        {
            return candidates.Take(topK).ToArray(); // nothing usable → keep upstream order
        }

        // Reorder candidates by the gateway's relevance ranking; guard index bounds.
        var reranked = results
            .OrderByDescending(r => r.RelevanceScore)
            .Where(r => r.Index >= 0 && r.Index < candidates.Count)
            .Select(r => candidates[r.Index])
            .Take(topK)
            .ToArray();

        return reranked.Length > 0 ? reranked : candidates.Take(topK).ToArray();
    }

    private sealed record RerankResponse([property: JsonPropertyName("results")] List<RerankItem>? Results);

    private sealed record RerankItem(
        [property: JsonPropertyName("index")] int Index,
        [property: JsonPropertyName("relevance_score")] double RelevanceScore);
}
