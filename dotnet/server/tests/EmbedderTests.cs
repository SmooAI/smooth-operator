using System.Net;
using System.Text;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Tests for the embedders. The deterministic one is asserted directly; the gateway one is driven
/// against a fake HTTP handler (canned OpenAI-compatible /embeddings response) so its real
/// request/parse logic runs in CI without a gateway.
/// </summary>
public class EmbedderTests
{
    [Fact]
    public async Task Deterministic_SameText_SameVector_DifferentText_Differs()
    {
        var embedder = new DeterministicEmbedder(64);
        var a1 = await embedder.EmbedAsync("the return window is 17 days");
        var a2 = await embedder.EmbedAsync("the return window is 17 days");
        var b = await embedder.EmbedAsync("shipping takes five business days");

        Assert.Equal(64, a1.Length);
        Assert.Equal(a1, a2);            // deterministic
        Assert.NotEqual(a1, b);          // different text → different vector
    }

    [Fact]
    public async Task Gateway_PostsToEmbeddings_AndReturnsTheVector()
    {
        const string responseJson = """{"data":[{"embedding":[0.10,0.20,0.30,0.40],"index":0}],"model":"text-embedding-3-small"}""";
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
        var embedder = new GatewayEmbedder(http, "text-embedding-3-small", dimensions: 4);

        var vector = await embedder.EmbedAsync("how long is the return window?");

        Assert.Equal(new[] { 0.10f, 0.20f, 0.30f, 0.40f }, vector);
        Assert.Equal(HttpMethod.Post, method);
        Assert.EndsWith("/embeddings", path);
        Assert.Contains("how long is the return window?", body);
        Assert.Contains("text-embedding-3-small", body);
    }

    private sealed class FakeHttpHandler : HttpMessageHandler
    {
        private readonly Func<HttpRequestMessage, HttpResponseMessage> _responder;

        public FakeHttpHandler(Func<HttpRequestMessage, HttpResponseMessage> responder) => _responder = responder;

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken) =>
            Task.FromResult(_responder(request));
    }
}
