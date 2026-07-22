using System.Net;
using System.Text;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Tests for the Notion connector (against a fake HTTP handler, so the real connector logic runs in
/// CI without hitting Notion): rich_text flattening across the supported block types, child_page
/// recursion producing a SEPARATE document (not inlined), id = page id / source = page URL stability,
/// pagination of a page's children, and per-root ACL label application (both on the emitted document
/// and end-to-end through the ACL-aware store).
/// </summary>
public class NotionConnectorTests
{
    private static readonly string[] Hr = { "notion:hr" };

    // Root page with one of every flattened block type, a nested toggle body, and a child_page.
    private const string RootChildrenJson = """
        {"results":[
          {"type":"heading_1","heading_1":{"rich_text":[{"plain_text":"Getting Started"}]}},
          {"type":"paragraph","paragraph":{"rich_text":[{"plain_text":"Welcome to the "},{"plain_text":"handbook."}]}},
          {"type":"bulleted_list_item","bulleted_list_item":{"rich_text":[{"plain_text":"First point"}]}},
          {"type":"numbered_list_item","numbered_list_item":{"rich_text":[{"plain_text":"Step one"}]}},
          {"type":"quote","quote":{"rich_text":[{"plain_text":"A wise quote"}]}},
          {"type":"code","code":{"rich_text":[{"plain_text":"print('hi')"}]}},
          {"type":"toggle","has_children":true,"id":"toggle1","toggle":{"rich_text":[{"plain_text":"Toggle header"}]}},
          {"type":"child_page","id":"child1","child_page":{"title":"Sub Page"}}
        ],"has_more":false,"next_cursor":null}
        """;

    private const string ToggleChildrenJson = """
        {"results":[{"type":"paragraph","paragraph":{"rich_text":[{"plain_text":"Hidden toggle body"}]}}],"has_more":false,"next_cursor":null}
        """;

    private const string ChildPageChildrenJson = """
        {"results":[{"type":"paragraph","paragraph":{"rich_text":[{"plain_text":"This is the sub page content."}]}}],"has_more":false,"next_cursor":null}
        """;

    private static NotionConnector Connect(Func<string, string?> childrenByBlockId, params NotionRoot[] roots)
    {
        var handler = new FakeHttpHandler(request =>
        {
            var url = request.RequestUri!.ToString();
            // Every request must carry the pinned Notion-Version header.
            Assert.True(request.Headers.Contains("Notion-Version"), "request missing Notion-Version header");
            var blockId = BlockIdFromUrl(url);
            var body = childrenByBlockId(blockId);
            return body is null
                ? new HttpResponseMessage(HttpStatusCode.NotFound)
                : Json(body);
        });
        return new NotionConnector(roots, new HttpClient(handler));
    }

    private static string BlockIdFromUrl(string url)
    {
        // .../v1/blocks/{id}/children?...
        const string marker = "/blocks/";
        var start = url.IndexOf(marker, StringComparison.Ordinal) + marker.Length;
        var end = url.IndexOf("/children", start, StringComparison.Ordinal);
        return url[start..end];
    }

    [Fact]
    public async Task FlattensSupportedBlockTypes_IncludingNestedToggleBody()
    {
        var connector = Connect(
            id => id switch { "root" => RootChildrenJson, "toggle1" => ToggleChildrenJson, "child1" => ChildPageChildrenJson, _ => null },
            new NotionRoot("root", Hr));

        var docs = await PullAll(connector);
        var root = docs.Single(d => d.Id == "root");

        foreach (var expected in new[] { "Getting Started", "Welcome to the handbook.", "First point", "Step one", "A wise quote", "print('hi')", "Toggle header", "Hidden toggle body" })
        {
            Assert.Contains(expected, root.Content);
        }
    }

    [Fact]
    public async Task ChildPage_BecomesSeparateDocument_NotInlinedInParent()
    {
        var connector = Connect(
            id => id switch { "root" => RootChildrenJson, "toggle1" => ToggleChildrenJson, "child1" => ChildPageChildrenJson, _ => null },
            new NotionRoot("root", Hr));

        var docs = await PullAll(connector);

        Assert.Equal(2, docs.Count); // the root page + the child_page, as distinct documents
        var root = docs.Single(d => d.Id == "root");
        var child = docs.Single(d => d.Id == "child1");
        Assert.DoesNotContain("This is the sub page content.", root.Content); // NOT inlined into the parent
        Assert.Contains("This is the sub page content.", child.Content);
    }

    [Fact]
    public async Task DocumentId_IsCanonicalPageId_AndSource_IsPageUrl_StableAcrossDashedInput()
    {
        // A dashed UUID root must normalize to the same dash-free id + URL a dash-free input would give.
        const string dashed = "11111111-2222-3333-4444-555555555555";
        const string canonical = "11111111222233334444555555555555";
        var connector = Connect(
            id => id == canonical
                ? """{"results":[{"type":"paragraph","paragraph":{"rich_text":[{"plain_text":"body"}]}}],"has_more":false,"next_cursor":null}"""
                : null,
            new NotionRoot(dashed, Hr));

        var doc = Assert.Single(await PullAll(connector));

        Assert.Equal(canonical, doc.Id); // stable id = the Notion page id (canonical form)
        Assert.Equal($"https://www.notion.so/{canonical}", doc.Source); // source = page URL, so citations link back
    }

    [Fact]
    public async Task PaginatesChildren_AcrossCursorPages()
    {
        var page1 = """{"results":[{"type":"paragraph","paragraph":{"rich_text":[{"plain_text":"page one body"}]}}],"has_more":true,"next_cursor":"cur2"}""";
        var page2 = """{"results":[{"type":"paragraph","paragraph":{"rich_text":[{"plain_text":"page two body"}]}}],"has_more":false,"next_cursor":null}""";
        var handler = new FakeHttpHandler(request =>
            Json(request.RequestUri!.ToString().Contains("start_cursor=cur2") ? page2 : page1));
        var connector = new NotionConnector(new[] { new NotionRoot("root", Hr) }, new HttpClient(handler));

        var doc = Assert.Single(await PullAll(connector));

        Assert.Contains("page one body", doc.Content);
        Assert.Contains("page two body", doc.Content); // the second cursor page was fetched and merged
    }

    [Fact]
    public async Task StampsPerRootAclLabel_OnEveryDocumentUnderThatRoot()
    {
        var connector = Connect(
            id => id switch
            {
                "hrroot" => """{"results":[{"type":"child_page","id":"hrsub","child_page":{"title":"Sub"}}],"has_more":false,"next_cursor":null}""",
                "hrsub" => """{"results":[{"type":"paragraph","paragraph":{"rich_text":[{"plain_text":"hr sub body"}]}}],"has_more":false,"next_cursor":null}""",
                "engroot" => """{"results":[{"type":"paragraph","paragraph":{"rich_text":[{"plain_text":"eng body"}]}}],"has_more":false,"next_cursor":null}""",
                _ => null,
            },
            new NotionRoot("hrroot", Hr),
            new NotionRoot("engroot", new[] { "notion:eng" }));

        var docs = await PullAll(connector);

        // Both the HR root page AND its child page inherit the HR root's ACL label; the eng root gets eng's.
        foreach (var id in new[] { "hrroot", "hrsub" })
        {
            Assert.Equal(new[] { "notion:hr" }, docs.Single(d => d.Id == id).Acl);
        }
        Assert.Equal(new[] { "notion:eng" }, docs.Single(d => d.Id == "engroot").Acl);
    }

    [Fact]
    public void WithDefaultLabel_DerivesCanonicalNotionRootLabel()
    {
        var root = NotionRoot.WithDefaultLabel("1111-2222");
        Assert.Equal(new[] { "notion:root:11112222" }, root.AclLabels);
    }

    [Fact]
    public async Task PerRootAcl_EnforcedEndToEnd_ThroughAclKnowledgeStore()
    {
        var connector = Connect(
            id => id == "hrroot"
                ? """{"results":[{"type":"paragraph","paragraph":{"rich_text":[{"plain_text":"the salary band is confidential"}]}}],"has_more":false,"next_cursor":null}"""
                : null,
            new NotionRoot("hrroot", Hr));

        var store = new AclKnowledgeStore();
        await foreach (var doc in connector.PullAsync())
        {
            // A consumer maps the connector's per-doc ACL labels onto a DocumentAcl at ingest.
            await store.IngestAsync(new KnowledgeDocument(doc.Id, doc.Content, doc.Source, doc.DocType), DocumentAcl.ForGroups(doc.Acl!.ToArray()));
        }

        var entitled = await store.ForAccess(Ctx("notion:hr"))!.QueryAsync("salary band", 4);
        var unentitled = await store.ForAccess(Ctx("notion:eng"))!.QueryAsync("salary band", 4);

        Assert.Contains(entitled, h => h.Chunk.Contains("salary band"));
        Assert.Empty(unentitled); // fail-closed: a non-HR caller never sees the HR-only page
    }

    private static AccessContext Ctx(params string[] groups) =>
        new(new Principal("u", "org", "member", groups), false);

    private static async Task<List<SourceDocument>> PullAll(NotionConnector connector)
    {
        var docs = new List<SourceDocument>();
        await foreach (var doc in connector.PullAsync())
        {
            docs.Add(doc);
        }
        return docs;
    }

    private static HttpResponseMessage Json(string body) =>
        new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };

    private sealed class FakeHttpHandler : HttpMessageHandler
    {
        private readonly Func<HttpRequestMessage, HttpResponseMessage> _responder;

        public FakeHttpHandler(Func<HttpRequestMessage, HttpResponseMessage> responder) => _responder = responder;

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken) =>
            Task.FromResult(_responder(request));
    }
}
