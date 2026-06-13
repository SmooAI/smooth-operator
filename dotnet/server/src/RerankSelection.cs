using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>Which rerank stage the operator wants. <see cref="Off"/> is the default, so the rerank
/// stage stays opt-in and default behavior is unchanged. Driven by <c>SMOOTH_AGENT_RERANK</c>.</summary>
public enum RerankMode
{
    /// <summary>Rerank disabled — no reorder (default).</summary>
    Off,

    /// <summary>Gateway cross-encoder if keyed, else fall back to the offline lexical reranker.</summary>
    Gateway,

    /// <summary>Force the offline deterministic lexical reranker (no network).</summary>
    Lexical,
}

/// <summary>
/// Selects the reranker for the retrieval path from configuration. The C# analog of the Rust
/// server's <c>build_reranker</c>: returns <c>null</c> when rerank is off (the default, so behavior
/// is unchanged), the real <see cref="GatewayReranker"/> only when gateway mode has a key, and the
/// offline <see cref="LexicalReranker"/> otherwise. Never makes an unauthenticated gateway call.
/// </summary>
public static class RerankSelection
{
    /// <summary>The default rerank model when none is configured.</summary>
    public const string DefaultRerankModel = "rerank-english-v3.0";

    /// <summary>Parse the <c>SMOOTH_AGENT_RERANK</c> value. Unknown/empty ⇒ <see cref="RerankMode.Off"/>.</summary>
    public static RerankMode ParseMode(string? value) => value?.Trim().ToLowerInvariant() switch
    {
        "gateway" or "on" or "1" or "true" => RerankMode.Gateway,
        "lexical" => RerankMode.Lexical,
        _ => RerankMode.Off,
    };

    /// <summary>
    /// Build the reranker for the configured mode. <paramref name="gatewayClientFactory"/> is
    /// invoked only when a real <see cref="GatewayReranker"/> is selected (gateway mode + a
    /// non-empty key), so callers that lack a gateway never pay to construct a client.
    /// </summary>
    public static IReranker? Build(RerankMode mode, bool hasGatewayKey, string model, Func<HttpClient> gatewayClientFactory) => mode switch
    {
        RerankMode.Off => null,
        RerankMode.Gateway => hasGatewayKey
            ? new GatewayReranker(gatewayClientFactory(), model)
            : new LexicalReranker(), // requested but unkeyed → offline fallback, never an unauth call
        RerankMode.Lexical => new LexicalReranker(),
        _ => null,
    };
}
