namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Per-turn token/iteration limits threaded into the agent config. Raised from the starvation-prone
/// legacy defaults (<c>max_tokens</c> 512, iterations 6) that made reasoning models return EMPTY — a
/// reasoning model needs headroom to think AND answer, and more than a handful of iterations to
/// actually use its tools (EPIC th-1cc9fa, matching the Rust server's <c>DEFAULT_MAX_TOKENS</c> /
/// <c>DEFAULT_MAX_ITERATIONS</c> raise). <see cref="ModelMaxOutputTokens"/> is the resolved model's
/// HARD output ceiling (from the gateway's <c>/model/info</c>); <c>null</c> ⇒ the engine leaves
/// <c>max_tokens</c> unclamped. A distinct carrier type (like <c>ConfirmTools</c>) so it can't collide
/// with other values in the DI container.
/// </summary>
public sealed record TurnLimits(
    int MaxTokens = TurnLimits.DefaultMaxTokens,
    int MaxIterations = TurnLimits.DefaultMaxIterations,
    int? ModelMaxOutputTokens = null)
{
    /// <summary>Default per-turn output-token budget. Raised 512 → 8192 (EPIC th-1cc9fa).</summary>
    public const int DefaultMaxTokens = 8_192;

    /// <summary>Default per-turn agentic-iteration cap. Raised 6 → 20 (EPIC th-1cc9fa).</summary>
    public const int DefaultMaxIterations = 20;

    /// <summary>The "raised server defaults, no model ceiling resolved" instance.</summary>
    public static readonly TurnLimits Default = new();
}
