using System.Net;
using System.Text;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Phase-3 tests: the chunker, the connector → chunk → store ingest pipeline (against the
/// MockConnector, the Rust connector-contract pattern), and the GitHub connector (against a fake
/// HTTP handler, so the real connector logic runs in CI without hitting GitHub).
/// </summary>
public class IngestionTests
{
    [Fact]
    public void Chunker_ShortContent_IsOneChunk()
    {
        var chunks = Chunker.Chunk("a short doc", new ChunkingOptions());
        Assert.Single(chunks);
        Assert.Equal("a short doc", chunks[0]);
    }

    [Fact]
    public void Chunker_LongNonWhitespaceRun_TerminatesAndCovers()
    {
        // Regression: a long run of non-whitespace (minified code / base64 / long URL) where the
        // only space sits within OverlapChars of a window start used to send `start` backward
        // (end - overlap <= start) → infinite loop. The chunker must terminate and cover the content.
        var content = "ab " + new string('x', 5000); // one early space, then a 5000-char no-space run
        var options = new ChunkingOptions(MaxChars: 100, OverlapChars: 20);

        var chunks = Chunker.Chunk(content, options); // must not hang

        Assert.True(chunks.Count > 1);
        Assert.All(chunks, c => Assert.True(c.Length <= options.MaxChars));
        Assert.Contains(chunks, c => c.Contains("xxxxx")); // the no-space run is captured
    }

    [Fact]
    public void Chunker_LongContent_SplitsWithBoundedSizeAndOverlap()
    {
        var word = string.Concat(Enumerable.Repeat("lorem ", 600)); // ~3600 chars
        var options = new ChunkingOptions(MaxChars: 1000, OverlapChars: 100);

        var chunks = Chunker.Chunk(word, options);

        Assert.True(chunks.Count > 1, "long content should split");
        Assert.All(chunks, c => Assert.True(c.Length <= options.MaxChars));
        // Overlap: the end of chunk 0 reappears at the start of chunk 1.
        var tail = chunks[0][^50..];
        Assert.Contains(tail.Split(' ', StringSplitOptions.RemoveEmptyEntries)[^1], chunks[1]);
    }

    [Fact]
    public async Task Pipeline_IngestsMockConnectorDocs_AndTheyAreQueryable()
    {
        var connector = new MockConnector(
            new SourceDocument("d1", "policies/returns.md", "The return window is 17 days from delivery."),
            new SourceDocument("d2", "policies/shipping.md", "Standard shipping takes 5 to 7 business days."));
        var kb = new InMemoryKnowledgeBase();
        var pipeline = new IngestPipeline(kb);

        var result = await pipeline.IngestAsync(connector);

        Assert.Equal(2, result.Documents);
        Assert.Equal(2, result.Chunks); // each short doc → one chunk
        var hits = await kb.QueryAsync("how long is the return window", 4);
        Assert.Contains(hits, h => h.Chunk.Contains("17 days"));
    }

    [Fact]
    public async Task Pipeline_ChunksLargeDocs_IntoMultipleRetrievableChunks()
    {
        var big = string.Concat(Enumerable.Repeat("filler ", 400)) + " the secret code is platypus.";
        var connector = new MockConnector(new SourceDocument("big", "big.md", big));
        var kb = new InMemoryKnowledgeBase();
        var pipeline = new IngestPipeline(kb, new ChunkingOptions(MaxChars: 500, OverlapChars: 50));

        var result = await pipeline.IngestAsync(connector);

        Assert.Equal(1, result.Documents);
        Assert.True(result.Chunks > 1, "a large doc should produce multiple chunks");
        var hits = await kb.QueryAsync("secret code platypus", 4);
        Assert.Contains(hits, h => h.Chunk.Contains("platypus"));
    }

    [Fact]
    public async Task GitHubConnector_ListsTree_FetchesTextFiles_SkipsBinaryAndTrees()
    {
        const string treeJson = """
            {"tree":[
              {"path":"README.md","type":"blob"},
              {"path":"src/App.cs","type":"blob"},
              {"path":"assets/logo.png","type":"blob"},
              {"path":"src","type":"tree"}
            ]}
            """;
        var handler = new FakeHttpHandler(request =>
        {
            var url = request.RequestUri!.ToString();
            if (url.Contains("api.github.com") && url.Contains("git/trees"))
            {
                return Json(treeJson);
            }
            if (url.EndsWith("README.md"))
            {
                return Text("# Returns\nThe return window is 17 days.");
            }
            if (url.EndsWith("App.cs"))
            {
                return Text("public class App { }");
            }
            return new HttpResponseMessage(HttpStatusCode.NotFound);
        });

        var connector = new GitHubConnector("acme", "handbook", new HttpClient(handler), "main");

        var docs = new List<SourceDocument>();
        await foreach (var doc in connector.PullAsync())
        {
            docs.Add(doc);
        }

        Assert.Equal(2, docs.Count); // README.md + App.cs; logo.png + the tree entry are skipped
        var readme = docs.Single(d => d.Source.EndsWith("README.md"));
        Assert.Contains("17 days", readme.Content);
        Assert.Equal(DocumentType.Markdown, readme.DocType);
        Assert.Equal(DocumentType.Code, docs.Single(d => d.Source.EndsWith("App.cs")).DocType);
    }

    [Fact]
    public async Task GitHubConnector_ThroughPipeline_GroundsAnswers()
    {
        const string treeJson = """{"tree":[{"path":"docs/returns.md","type":"blob"}]}""";
        var handler = new FakeHttpHandler(request =>
            request.RequestUri!.ToString().Contains("git/trees")
                ? Json(treeJson)
                : Text("Our return window is 17 days from delivery, for a full refund."));
        var connector = new GitHubConnector("acme", "handbook", new HttpClient(handler));
        var kb = new InMemoryKnowledgeBase();

        var result = await new IngestPipeline(kb).IngestAsync(connector);

        Assert.Equal(1, result.Documents);
        var hits = await kb.QueryAsync("return window", 4);
        Assert.Contains(hits, h => h.Chunk.Contains("17 days"));
    }

    private static HttpResponseMessage Json(string body) =>
        new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };

    private static HttpResponseMessage Text(string body) =>
        new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "text/plain") };

    private sealed class FakeHttpHandler : HttpMessageHandler
    {
        private readonly Func<HttpRequestMessage, HttpResponseMessage> _responder;

        public FakeHttpHandler(Func<HttpRequestMessage, HttpResponseMessage> responder) => _responder = responder;

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken) =>
            Task.FromResult(_responder(request));
    }
}
