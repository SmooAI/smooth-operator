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
    // ---- Chunker: parity with the Rust ingestion chunker (500/64 paragraph-aware, "{id}#{index}") ----

    [Fact]
    public void Chunker_DefaultOptions_MatchTheRustSpec()
    {
        var options = new ChunkingOptions();
        Assert.Equal(500, options.MaxChars);
        Assert.Equal(64, options.OverlapChars);
    }

    [Fact]
    public void Chunker_TinyDoc_IsOneChunk_WithStableId()
    {
        var doc = new SourceDocument("d", "test", "just a short note");
        var chunks = Chunker.Chunk(doc, new ChunkingOptions());
        Assert.Single(chunks);
        Assert.Equal("just a short note", chunks[0].Text);
        Assert.Equal("d#0", chunks[0].Id);
        Assert.Equal(0, chunks[0].Index);
        Assert.Equal("d", chunks[0].DocumentId);
    }

    [Fact]
    public void Chunker_EmptyDoc_YieldsNoChunks()
    {
        var doc = new SourceDocument("d", "test", "   \n\n   ");
        Assert.Empty(Chunker.Chunk(doc, new ChunkingOptions()));
    }

    [Fact]
    public void Chunker_Paragraphs_PackThenSplitAtCap()
    {
        // max 20, no overlap → each ~15-char paragraph is its own chunk (15 + 2 + 15 > 20).
        var doc = new SourceDocument("d", "test", "paragraph one!!\n\nparagraph two!!\n\nparagraph thr!!");
        var chunks = Chunker.Chunk(doc, new ChunkingOptions(MaxChars: 20, OverlapChars: 0));
        Assert.Equal(3, chunks.Count);
        Assert.Equal("paragraph one!!", chunks[0].Text);
        Assert.Equal("paragraph two!!", chunks[1].Text);
        Assert.Equal(2, chunks[2].Index);
    }

    [Fact]
    public void Chunker_SmallParagraphs_PackIntoOneChunk()
    {
        var doc = new SourceDocument("d", "test", "aaa\n\nbbb\n\nccc");
        var chunks = Chunker.Chunk(doc, new ChunkingOptions(MaxChars: 100, OverlapChars: 0));
        Assert.Single(chunks);
        Assert.Contains("aaa", chunks[0].Text);
        Assert.Contains("ccc", chunks[0].Text);
    }

    [Fact]
    public void Chunker_OversizedParagraph_HardSplitsOnWords_UnderCap()
    {
        var doc = new SourceDocument("d", "test", "alpha beta gamma delta epsilon");
        var chunks = Chunker.Chunk(doc, new ChunkingOptions(MaxChars: 10, OverlapChars: 0));
        Assert.True(chunks.Count > 1, "oversized paragraph must split");
        Assert.All(chunks, c => Assert.True(c.Text.Length <= 10, $"chunk exceeds cap: {c.Text}"));
        // Word boundaries preserved — no word is cut in half.
        var joined = string.Join(' ', chunks.Select(c => c.Text));
        Assert.Equal("alpha beta gamma delta epsilon", joined);
    }

    [Fact]
    public void Chunker_LongContent_SplitsWithBoundedSize()
    {
        var word = string.Concat(Enumerable.Repeat("lorem ", 600)); // ~3600 chars, one paragraph
        var chunks = Chunker.Chunk(new SourceDocument("d", "test", word), new ChunkingOptions(MaxChars: 1000, OverlapChars: 0));
        Assert.True(chunks.Count > 1, "long content should split");
        Assert.All(chunks, c => Assert.True(c.Text.Length <= 1000));
    }

    [Fact]
    public void Chunker_Overlap_CarriesTrailingWholeWordsIntoNextChunk()
    {
        var doc = new SourceDocument("d", "test", "first chunk text\n\nsecond chunk text\n\nthird chunk text");
        var chunks = Chunker.Chunk(doc, new ChunkingOptions(MaxChars: 20, OverlapChars: 8));
        Assert.True(chunks.Count >= 2);
        var prevLast = chunks[0].Text.Split(' ', StringSplitOptions.RemoveEmptyEntries)[^1];
        Assert.StartsWith(prevLast, chunks[1].Text);
    }

    [Fact]
    public void Chunker_Overlap_IsWholeWord_AndWithinBudget()
    {
        // Two paragraphs at cap 20, overlap 8. The prefix carried into chunk[1] must be whole words
        // whose char length is ≤ the overlap budget (8).
        var doc = new SourceDocument("d", "test", "alpha bravo charlie\n\ndelta echo foxtrot");
        var chunks = Chunker.Chunk(doc, new ChunkingOptions(MaxChars: 20, OverlapChars: 8));
        Assert.True(chunks.Count >= 2);
        // chunk[1] = "<overlap> delta echo foxtrot"; strip the original tail to isolate the overlap.
        const string original = "delta echo foxtrot";
        Assert.EndsWith(original, chunks[1].Text);
        var overlap = chunks[1].Text[..^original.Length].TrimEnd();
        Assert.True(overlap.Length <= 8, $"overlap '{overlap}' exceeds 8-char budget");
        // Whole words only: every overlap token is a whole word from the previous chunk.
        var prevWords = chunks[0].Text.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        foreach (var w in overlap.Split(' ', StringSplitOptions.RemoveEmptyEntries))
        {
            Assert.Contains(w, prevWords);
        }
    }

    [Fact]
    public void Chunker_OverlapClampedBelowMax_Terminates()
    {
        // overlap >= max would loop forever; the chunker clamps it. Must terminate and produce chunks.
        var doc = new SourceDocument("d", "test", "alpha beta gamma delta epsilon zeta");
        var chunks = Chunker.Chunk(doc, new ChunkingOptions(MaxChars: 10, OverlapChars: 999));
        Assert.NotEmpty(chunks);
    }

    [Fact]
    public void Chunker_ChunkIds_AreDocIndexFormat_AndStable()
    {
        var doc = new SourceDocument("doc-42", "test", "alpha words!!\n\nbeta words!!");
        var chunks = Chunker.Chunk(doc, new ChunkingOptions(MaxChars: 15, OverlapChars: 0));
        Assert.Equal("doc-42#0", chunks[0].Id);
        Assert.Equal("doc-42#1", chunks[1].Id);
        // Re-chunking the same input yields the same chunks (stable).
        var again = Chunker.Chunk(doc, new ChunkingOptions(MaxChars: 15, OverlapChars: 0));
        Assert.Equal(chunks, again);
    }

    // ---- IngestLedger: content-hash idempotency (parity with the Rust ledger) ----

    [Fact]
    public void Ledger_ContentHash_IsStable_AndMatchesRustFnv1a()
    {
        // FNV-1a 64-bit offset basis, so the empty string locks the algorithm constants to Rust's.
        Assert.Equal("cbf29ce484222325", IngestLedger.ContentHash(string.Empty));
        Assert.Equal(16, IngestLedger.ContentHash("anything").Length);
        Assert.Equal(IngestLedger.ContentHash("same text"), IngestLedger.ContentHash("same text"));
        Assert.NotEqual(IngestLedger.ContentHash("text a"), IngestLedger.ContentHash("text b"));
    }

    [Fact]
    public void Ledger_Record_IsIdempotent_AndProbeDoesNotRecord()
    {
        var ledger = new IngestLedger();
        Assert.True(ledger.IsEmpty);
        var key = IngestLedger.KeyFor("d1", "hello");

        Assert.False(ledger.Contains(key)); // probe: does not record
        Assert.Equal(0, ledger.Count);
        Assert.True(ledger.Record(key));     // newly inserted
        Assert.False(ledger.Record(key));    // already present
        Assert.True(ledger.Contains(key));
        Assert.Equal(1, ledger.Count);
        Assert.False(ledger.IsEmpty);
    }

    [Fact]
    public void Ledger_KeyFor_DiffersByDocAndContent()
    {
        Assert.NotEqual(IngestLedger.KeyFor("d1", "x"), IngestLedger.KeyFor("d2", "x")); // doc differs
        Assert.NotEqual(IngestLedger.KeyFor("d1", "x"), IngestLedger.KeyFor("d1", "y")); // content differs
        Assert.Equal(IngestLedger.KeyFor("d1", "x"), IngestLedger.KeyFor("d1", "x"));    // same → same
    }

    [Fact]
    public async Task Pipeline_ReingestingIdenticalContent_IsNoOp()
    {
        var ledger = new IngestLedger();
        var connector = new MockConnector(
            new SourceDocument("d1", "a.md", "The return window is 17 days from delivery."),
            new SourceDocument("d2", "b.md", "Standard shipping takes 5 to 7 business days."));
        var kb = new InMemoryKnowledgeBase();
        var pipeline = new IngestPipeline(kb, ledger: ledger);

        var first = await pipeline.IngestAsync(connector);
        Assert.Equal(2, first.Chunks);
        Assert.Equal(0, first.SkippedDocuments);

        // Same connector, same shared ledger → nothing new is stored.
        var second = await pipeline.IngestAsync(connector);
        Assert.Equal(0, second.Chunks);
        Assert.Equal(2, second.SkippedDocuments);
        Assert.Equal(2, second.Documents); // still pulled, just skipped
    }

    [Fact]
    public async Task Pipeline_ChangedContent_IsReprocessed()
    {
        var ledger = new IngestLedger();
        var kb = new InMemoryKnowledgeBase();

        var v1 = await new IngestPipeline(kb, ledger: ledger)
            .IngestAsync(new MockConnector(new SourceDocument("d1", "a.md", "The return window is 17 days.")));
        Assert.Equal(1, v1.Chunks);

        // Same doc id, different content → a new content hash → reprocessed, not skipped.
        var v2 = await new IngestPipeline(kb, ledger: ledger)
            .IngestAsync(new MockConnector(new SourceDocument("d1", "a.md", "The return window is now 30 days.")));
        Assert.Equal(1, v2.Chunks);
        Assert.Equal(0, v2.SkippedDocuments);

        var hits = await kb.QueryAsync("return window", 4);
        Assert.Contains(hits, h => h.Chunk.Contains("30 days"));
    }

    [Fact]
    public async Task Pipeline_StoresChunkIds_InDocIndexFormat()
    {
        var recording = new RecordingKnowledgeBase();
        // Distinct tokens so no two chunks share a content hash (identical chunks would dedupe, which
        // is correct but would leave the stored ids non-contiguous — not what this test is checking).
        var big = string.Join(' ', Enumerable.Range(0, 400).Select(i => $"word{i}"));
        var pipeline = new IngestPipeline(recording, new ChunkingOptions(MaxChars: 500, OverlapChars: 0));

        await pipeline.IngestAsync(new MockConnector(new SourceDocument("big", "big.md", big)));

        Assert.True(recording.Ids.Count > 1, "large doc should produce multiple chunks");
        for (var i = 0; i < recording.Ids.Count; i++)
        {
            Assert.Equal($"big#{i}", recording.Ids[i]);
        }
    }

    /// <summary>An <see cref="IKnowledgeBase"/> that just records the ids it was asked to ingest.</summary>
    private sealed class RecordingKnowledgeBase : IKnowledgeBase
    {
        public List<string> Ids { get; } = new();

        public Task IngestAsync(KnowledgeDocument document, CancellationToken cancellationToken = default)
        {
            Ids.Add(document.Id);
            return Task.CompletedTask;
        }

        public Task<IReadOnlyList<KnowledgeResult>> QueryAsync(string query, int limit, CancellationToken cancellationToken = default) =>
            Task.FromResult<IReadOnlyList<KnowledgeResult>>(Array.Empty<KnowledgeResult>());
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

    [Fact]
    public async Task GitHubConnector_TruncatedTree_FailsLoud_NotSilentlyPartial()
    {
        // GitHub returns truncated=true (partial tree) for huge repos. Ingesting that silently would
        // build an incomplete index that reports success; the connector must fail loud instead.
        const string treeJson = """{"truncated":true,"tree":[{"path":"README.md","type":"blob"}]}""";
        var handler = new FakeHttpHandler(request =>
            request.RequestUri!.ToString().Contains("git/trees") ? Json(treeJson) : Text("partial"));
        var connector = new GitHubConnector("acme", "huge", new HttpClient(handler));

        var ex = await Assert.ThrowsAsync<InvalidOperationException>(async () =>
        {
            await foreach (var _ in connector.PullAsync())
            {
            }
        });
        Assert.Contains("truncated", ex.Message, StringComparison.OrdinalIgnoreCase);
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
