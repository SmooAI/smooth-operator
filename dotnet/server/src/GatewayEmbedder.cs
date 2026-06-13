using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// A semantic <see cref="IEmbedder"/> backed by an OpenAI-compatible <c>/embeddings</c> endpoint
/// (the smooth gateway, Azure OpenAI, …). The C# analog of the Rust <c>GatewayEmbedder</c> — real
/// embeddings for quality retrieval, vs the deterministic bag-of-words fallback. The caller supplies
/// a configured <see cref="HttpClient"/> (BaseAddress = the gateway, with the auth header). The HTTP
/// path is unit-tested against a fake handler, so the logic runs in CI without a gateway.
/// </summary>
public sealed class GatewayEmbedder : IEmbedder
{
    private static readonly JsonSerializerOptions JsonOptions = new() { PropertyNameCaseInsensitive = true };

    private readonly HttpClient _http;
    private readonly string _model;

    /// <summary>The model's output dimension (e.g. 1536 for text-embedding-3-small) — sizes the vector column.</summary>
    public int Dimensions { get; }

    public GatewayEmbedder(HttpClient httpClient, string model, int dimensions = 1536)
    {
        _http = httpClient ?? throw new ArgumentNullException(nameof(httpClient));
        _model = model;
        Dimensions = dimensions;
    }

    public async Task<float[]> EmbedAsync(string text, CancellationToken cancellationToken = default)
    {
        var payload = JsonSerializer.Serialize(new { model = _model, input = text });
        using var content = new StringContent(payload, Encoding.UTF8, "application/json");
        using var response = await _http.PostAsync("embeddings", content, cancellationToken).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();

        await using var stream = await response.Content.ReadAsStreamAsync(cancellationToken).ConfigureAwait(false);
        var parsed = await JsonSerializer.DeserializeAsync<EmbeddingResponse>(stream, JsonOptions, cancellationToken).ConfigureAwait(false);

        var embedding = parsed?.Data is { Count: > 0 } data ? data[0].Embedding : null;
        if (embedding is null || embedding.Length == 0)
        {
            throw new InvalidOperationException("Embedding endpoint returned no vector.");
        }
        return embedding;
    }

    private sealed record EmbeddingResponse([property: JsonPropertyName("data")] List<EmbeddingItem>? Data);

    private sealed record EmbeddingItem([property: JsonPropertyName("embedding")] float[] Embedding);
}
