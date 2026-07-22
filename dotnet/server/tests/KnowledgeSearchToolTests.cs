using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Tests for the built-in <c>knowledge_search</c> tool: it registers under the gate-able name, is
/// callable, returns grounded results in the Rust tool's text shape, and — the load-bearing property —
/// only ever surfaces documents the caller's <see cref="AccessContext"/> is entitled to (it is built
/// over the ACL-scoped <see cref="IAccessKnowledge.ForAccess"/> handle, so a private doc outside the
/// caller's groups is never returned). Mirrors the Rust knowledge_search suite + the acl_chat_leak tests.
/// </summary>
public class KnowledgeSearchToolTests
{
    private static AclKnowledgeStore SeededStore()
    {
        var store = new AclKnowledgeStore();
        store.IngestAsync(new KnowledgeDocument("pub", "SmooAI returns are accepted within 30 days for a full refund.", "policies/returns.md"), DocumentAcl.PublicAcl);
        store.IngestAsync(new KnowledgeDocument("secret", "The private launch code is hunter2.", "acme/private/launch.md"),
            DocumentAcl.ForGroups("github:acme/private"));
        return store;
    }

    private static AccessContext WithGroups(params string[] groups) =>
        new(new Principal("u", "acme", "basic", groups), IsAnonymous: groups.Length == 0);

    private static AIFunction ToolFor(AccessContext access)
    {
        var tool = KnowledgeSearchTool.Create(SeededStore().ForAccess(access));
        Assert.NotNull(tool);
        return (AIFunction)tool!;
    }

    private static Task<object?> Invoke(AIFunction tool, string query, int? limit = null)
    {
        var args = new AIFunctionArguments { ["query"] = query };
        if (limit is not null)
        {
            args["limit"] = limit.Value;
        }
        return tool.InvokeAsync(args).AsTask();
    }

    [Fact]
    public void Create_NullKnowledge_ReturnsNull()
    {
        // No knowledge store configured ⇒ nothing to enable, so callers can prepend unconditionally.
        Assert.Null(KnowledgeSearchTool.Create(null));
    }

    [Fact]
    public void Tool_RegistersUnderGateableName()
    {
        var tool = ToolFor(AccessContext.Anonymous);
        Assert.Equal("knowledge_search", tool.Name);
        Assert.Equal(KnowledgeSearchTool.ToolName, tool.Name);
        Assert.False(string.IsNullOrWhiteSpace(tool.Description));
    }

    [Fact]
    public async Task Tool_IsCallable_ReturnsGroundedResult()
    {
        var tool = ToolFor(AccessContext.Anonymous);
        var result = (await Invoke(tool, "return policy refund"))?.ToString() ?? string.Empty;
        Assert.Contains("30 days", result, StringComparison.Ordinal);
        Assert.Contains("policies/returns.md", result, StringComparison.Ordinal);
    }

    [Fact]
    public async Task Tool_RespectsAclScoping_EntitledUserSeesPrivateDoc()
    {
        var tool = ToolFor(WithGroups("github:acme/private"));
        var result = (await Invoke(tool, "private launch code hunter2"))?.ToString() ?? string.Empty;
        Assert.Contains("hunter2", result, StringComparison.Ordinal);
    }

    [Fact]
    public async Task Tool_RespectsAclScoping_PrivateDocNotLeakedToUnentitledUser()
    {
        // A doc outside the caller's ACL must NOT be returned — the tool searches through the
        // access-scoped handle, so the private doc is never even a candidate. #1 adversarial leak.
        // Query with terms NOT in the secret's chunk (so a leak shows as the doc's source/content,
        // not a query echo in the empty-result message).
        var anon = ToolFor(AccessContext.Anonymous);
        var anonResult = (await Invoke(anon, "private launch code"))?.ToString() ?? string.Empty;
        Assert.DoesNotContain("hunter2", anonResult, StringComparison.Ordinal);
        Assert.DoesNotContain("acme/private", anonResult, StringComparison.Ordinal);

        var otherGroup = ToolFor(WithGroups("github:acme/other"));
        var otherResult = (await Invoke(otherGroup, "private launch code"))?.ToString() ?? string.Empty;
        Assert.DoesNotContain("hunter2", otherResult, StringComparison.Ordinal);
        Assert.DoesNotContain("acme/private", otherResult, StringComparison.Ordinal);
    }

    [Fact]
    public async Task Tool_NoMatch_ReportsEmpty()
    {
        var tool = ToolFor(AccessContext.Anonymous);
        var result = (await Invoke(tool, "warranty electronics voltage regulator"))?.ToString() ?? string.Empty;
        Assert.Contains("No knowledge base results", result, StringComparison.Ordinal);
    }

    [Fact]
    public void Format_MatchesRustTextShape()
    {
        var results = new List<KnowledgeResult>
        {
            new("doc-1", "First chunk.", 2.0, "a.md"),
            new("doc-2", "Second chunk.", 1.5, "b.md"),
        };
        var text = KnowledgeSearchTool.Format("hello world", results);
        Assert.Equal(
            "Found 2 knowledge base result(s) for \"hello world\":\n" +
            "1. [source=a.md | id=doc-1 | relevance=2.00]\nFirst chunk.\n" +
            "2. [source=b.md | id=doc-2 | relevance=1.50]\nSecond chunk.\n",
            text);
    }

    [Fact]
    public void Format_Empty_QuotesQueryLikeRustDebug()
    {
        Assert.Equal(
            "No knowledge base results found for query: \"nope\"",
            KnowledgeSearchTool.Format("nope", Array.Empty<KnowledgeResult>()));
    }
}
