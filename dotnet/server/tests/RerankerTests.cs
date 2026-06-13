using System.Net;
using System.Text;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Server-side reranker tests: the GatewayReranker reorders candidates by the gateway's
/// /rerank response (driven against a fake HTTP handler, CI-safe), and the selection logic mirrors
/// the Rust <c>build_reranker</c> (off→none, gateway+key→gateway, gateway-no-key→lexical, lexical→lexical).
/// </summary>
public class RerankerTests
{
    private static KnowledgeResult Result(string id, string chunk) => new(id, chunk, 0.5, $"{id}.md");

    [Fact]
    public async Task Gateway_ReordersByRelevance_FromRerankResponse()
    {
        // Upstream order: shipping, returns. The gateway says index 1 (returns) is most relevant.
        const string responseJson = """{"results":[{"index":1,"relevance_score":0.98},{"index":0,"relevance_score":0.12}]}""";
        HttpMethod? method = null;
        string? path = null;
        string? body = null;
        var handler = new FakeHttpHandler(request =>
        {
            method = request.Method;
            path = request.RequestUri!.AbsolutePath;
            body = request.Content!.ReadAsStringAsync().GetAwaiter().GetResult();
            return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(responseJson, Encoding.UTF8, "application/json") };
        });
        var http = new HttpClient(handler) { BaseAddress = new Uri("https://gateway.test/v1/") };
        var reranker = new GatewayReranker(http, "rerank-english-v3.0");

        var candidates = new[] { Result("shipping", "shipping and delivery times"), Result("returns", "refund window and return policy") };
        var reranked = await reranker.RerankAsync("what is the refund window?", candidates, 2);

        Assert.Equal(new[] { "returns", "shipping" }, reranked.Select(r => r.DocumentId)); // reordered by relevance
        Assert.Equal(HttpMethod.Post, method);
        Assert.EndsWith("/rerank", path);
        Assert.Contains("rerank-english-v3.0", body);
        Assert.Contains("refund window", body);
    }

    [Fact]
    public async Task Gateway_EmptyResults_PreservesUpstreamOrder()
    {
        var handler = new FakeHttpHandler(_ =>
            new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent("""{"results":[]}""", Encoding.UTF8, "application/json") });
        var http = new HttpClient(handler) { BaseAddress = new Uri("https://gateway.test/v1/") };
        var reranker = new GatewayReranker(http, "rerank-english-v3.0");

        var candidates = new[] { Result("a", "alpha"), Result("b", "beta") };
        var reranked = await reranker.RerankAsync("q", candidates, 2);

        Assert.Equal(new[] { "a", "b" }, reranked.Select(r => r.DocumentId));
    }

    [Theory]
    [InlineData("gateway", RerankMode.Gateway)]
    [InlineData("ON", RerankMode.Gateway)]
    [InlineData("1", RerankMode.Gateway)]
    [InlineData("true", RerankMode.Gateway)]
    [InlineData("lexical", RerankMode.Lexical)]
    [InlineData("off", RerankMode.Off)]
    [InlineData("", RerankMode.Off)]
    [InlineData("nonsense", RerankMode.Off)]
    [InlineData(null, RerankMode.Off)]
    public void ParseMode_MapsEnvValue(string? value, RerankMode expected) =>
        Assert.Equal(expected, RerankSelection.ParseMode(value));

    [Fact]
    public void Build_Off_ReturnsNull_AndNeverBuildsClient() =>
        Assert.Null(RerankSelection.Build(RerankMode.Off, hasGatewayKey: true, "m", ThrowingFactory));

    [Fact]
    public void Build_GatewayWithKey_ReturnsGatewayReranker()
    {
        var reranker = RerankSelection.Build(RerankMode.Gateway, hasGatewayKey: true, "m", () => new HttpClient { BaseAddress = new Uri("https://gw.test/v1/") });
        Assert.IsType<GatewayReranker>(reranker);
    }

    [Fact]
    public void Build_GatewayWithoutKey_FallsBackToLexical_AndNeverBuildsClient() =>
        Assert.IsType<LexicalReranker>(RerankSelection.Build(RerankMode.Gateway, hasGatewayKey: false, "m", ThrowingFactory));

    [Fact]
    public void Build_Lexical_ReturnsLexical_AndNeverBuildsClient() =>
        Assert.IsType<LexicalReranker>(RerankSelection.Build(RerankMode.Lexical, hasGatewayKey: true, "m", ThrowingFactory));

    private static HttpClient ThrowingFactory() => throw new InvalidOperationException("gateway client must not be constructed for this mode");

    private sealed class FakeHttpHandler : HttpMessageHandler
    {
        private readonly Func<HttpRequestMessage, HttpResponseMessage> _responder;

        public FakeHttpHandler(Func<HttpRequestMessage, HttpResponseMessage> responder) => _responder = responder;

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken) =>
            Task.FromResult(_responder(request));
    }
}
