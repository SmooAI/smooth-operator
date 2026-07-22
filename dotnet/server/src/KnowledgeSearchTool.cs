using System.ComponentModel;
using System.Globalization;
using System.Text;
using System.Text.Json;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// The built-in <c>knowledge_search</c> tool — the model's hands on the RAG knowledge base.
///
/// Registering an <see cref="IAccessKnowledge"/> gives the server RAG <em>grounding</em> (the turn
/// runner auto-injects a few top hits before the first LLM call). This tool exposes the same store as
/// a model-<em>callable</em> function, so the agent can decide to search mid-turn with its own phrasing.
/// Parity with the Rust server's <c>KnowledgeSearchTool</c>: same tool name, arguments
/// (<c>query</c> required + <c>limit</c> clamped to 1..10, default 3), and text result shape.
///
/// It is built over an <b>already ACL-scoped</b> <see cref="IKnowledgeBase"/> — the handle returned by
/// <see cref="IAccessKnowledge.ForAccess"/> for the connection's <see cref="AccessContext"/> — so every
/// search is document-level access-controlled: a document outside the caller's ACL is never a candidate,
/// exactly like retrieval on the auto-context path.
/// </summary>
public static class KnowledgeSearchTool
{
    /// <summary>The tool's name. Must equal this for <c>EnabledTools</c> name-gating to match it.</summary>
    public const string ToolName = "knowledge_search";

    private const string Description =
        "Search the organization's knowledge base for facts relevant to the user's question " +
        "(policies, product details, documentation). Returns the most relevant snippets with their " +
        "source and relevance score. Call this before answering any question that depends on " +
        "organization-specific knowledge.";

    private const int DefaultLimit = 3;
    private const int MinLimit = 1;
    private const int MaxLimit = 10;

    /// <summary>
    /// Build the tool over an ACL-scoped knowledge handle. Pass the result of
    /// <see cref="IAccessKnowledge.ForAccess"/> so results are already filtered to the caller's
    /// entitlements. Returns <c>null</c> when <paramref name="knowledge"/> is <c>null</c> (no store
    /// configured ⇒ no tool to enable), so callers can prepend the result unconditionally.
    /// </summary>
    public static AITool? Create(IKnowledgeBase? knowledge)
    {
        if (knowledge is null)
        {
            return null;
        }

        async Task<string> Search(
            [Description("The search query — phrase it with the key terms you expect to appear in the answer (e.g. 'return policy refund window').")]
            string query,
            [Description("Maximum number of snippets to return (default 3, clamped to 1..10).")]
            int limit = DefaultLimit,
            CancellationToken cancellationToken = default)
        {
            if (string.IsNullOrWhiteSpace(query))
            {
                throw new ArgumentException("knowledge_search requires a non-empty 'query' argument.", nameof(query));
            }

            var clamped = Math.Clamp(limit, MinLimit, MaxLimit);
            var results = await knowledge.QueryAsync(query, clamped, cancellationToken).ConfigureAwait(false);
            return Format(query, results);
        }

        return (AITool)AIFunctionFactory.Create(Search, new AIFunctionFactoryOptions
        {
            Name = ToolName,
            Description = Description,
        });
    }

    /// <summary>
    /// Render results in the Rust tool's exact text shape so both servers speak the same result
    /// format to the model. The query is quoted like Rust's <c>{query:?}</c> (JSON-escaped).
    /// </summary>
    public static string Format(string query, IReadOnlyList<KnowledgeResult> results)
    {
        // JSON-serialize the query to mirror Rust's Debug formatting of a string ({:?} ⇒ quoted+escaped).
        var quoted = JsonSerializer.Serialize(query);

        if (results.Count == 0)
        {
            return $"No knowledge base results found for query: {quoted}";
        }

        var sb = new StringBuilder();
        sb.Append(CultureInfo.InvariantCulture, $"Found {results.Count} knowledge base result(s) for {quoted}:\n");
        for (var i = 0; i < results.Count; i++)
        {
            var r = results[i];
            sb.Append(CultureInfo.InvariantCulture, $"{i + 1}. [source={r.Source} | id={r.DocumentId} | relevance={r.Score.ToString("0.00", CultureInfo.InvariantCulture)}]\n{r.Chunk}\n");
        }
        return sb.ToString();
    }
}
