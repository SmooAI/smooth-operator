using System.Net;
using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Parity tests for the model-output-token ceiling clamp on the server side (EPIC th-1cc9fa): the
/// gateway <c>/model/info</c> ceiling parse/fetch, the raised starvation defaults, and TurnRunner
/// threading the budget into the model request (where the engine clamps it to the ceiling).
/// </summary>
public class MaxTokensLimitsTests
{
    private const string SamplePayload = """
    {
      "data": [
        { "model_name": "groq-compound", "model_info": { "max_output_tokens": 8192, "input_cost_per_token": 0.0000001 } },
        { "model_name": "claude-haiku-4-5", "model_info": { "max_output_tokens": 65536 } },
        { "model_name": "no-ceiling-model", "model_info": { "input_cost_per_token": 0.0000002 } },
        { "model_name": "zero-ceiling-model", "model_info": { "max_output_tokens": 0 } },
        { "model_info": { "max_output_tokens": 1234 } }
      ]
    }
    """;

    // ── ParseCeilings (pure, network-free) ───────────────────────────────────────────────────────

    [Fact]
    public void ParseCeilings_MapsPositiveCeilingsByModelName()
    {
        var map = ModelInfo.ParseCeilings(JsonNode.Parse(SamplePayload));

        Assert.Equal(8192, map["groq-compound"]);
        Assert.Equal(65536, map["claude-haiku-4-5"]);
    }

    [Fact]
    public void ParseCeilings_DropsMissingZeroAndNamelessEntries()
    {
        var map = ModelInfo.ParseCeilings(JsonNode.Parse(SamplePayload));

        Assert.False(map.ContainsKey("no-ceiling-model"));  // no max_output_tokens
        Assert.False(map.ContainsKey("zero-ceiling-model")); // 0 is not a real ceiling
        Assert.Equal(2, map.Count);                          // the nameless entry is skipped too
    }

    [Fact]
    public void ParseCeilings_ToleratesMalformedPayloads()
    {
        Assert.Empty(ModelInfo.ParseCeilings(null));
        Assert.Empty(ModelInfo.ParseCeilings(JsonNode.Parse("{}")));
        Assert.Empty(ModelInfo.ParseCeilings(JsonNode.Parse("{\"data\": \"nope\"}")));
        Assert.Empty(ModelInfo.ParseCeilings(JsonNode.Parse("{\"data\": []}")));
        // A scalar entry / scalar model_info must read as "no ceiling", not throw — indexing a
        // non-object JsonNode raises, so these have to be coerced rather than indexed blind.
        Assert.Empty(ModelInfo.ParseCeilings(JsonNode.Parse("{\"data\": [7]}")));
        Assert.Empty(ModelInfo.ParseCeilings(JsonNode.Parse("{\"data\":[{\"model_name\":\"x\",\"model_info\":7}]}")));
        Assert.Empty(ModelInfo.ParseCeilings(JsonNode.Parse("{\"data\":[{\"model_name\":42}]}")));
    }

    // ── MapModelInfo (pure, network-free) — what GET /admin/model-costs answers ──────────────────

    private const string CostsPayload = """
    {
      "data": [
        {
          "model_name": "claude-opus-4-8",
          "model_info": {
            "input_cost_per_token": 0.000015,
            "output_cost_per_token": 0.000075,
            "model_tier": "frontier",
            "use_cases": ["reasoning", "coding"],
            "max_output_tokens": 65536
          }
        },
        {
          "model_name": "claude-haiku-4-5",
          "model_info": { "input_cost_per_token": 0.0000008, "model_tier": "fast", "use_cases": ["chat"] }
        },
        { "model_name": "bare", "model_info": {} },
        { "model_info": { "model_tier": "nameless" } }
      ]
    }
    """;

    [Fact]
    public void MapModelInfo_MapsCostsTierAndUseCasesByModelName()
    {
        var map = ModelInfo.MapModelInfo(JsonNode.Parse(CostsPayload));

        var opus = map["claude-opus-4-8"]!;
        Assert.Equal(0.000015, opus["inputCostPerToken"]!.GetValue<double>(), precision: 12);
        Assert.Equal(0.000075, opus["outputCostPerToken"]!.GetValue<double>(), precision: 12);
        Assert.Equal("frontier", opus["tier"]!.GetValue<string>());
        Assert.Equal(new[] { "reasoning", "coding" }, opus["useCases"]!.AsArray().Select(n => n!.GetValue<string>()));
        Assert.Equal(65536, opus["maxOutputTokens"]!.GetValue<double>());
    }

    /// <summary>
    /// The property that matters for the console: an omitted field is null, NOT defaulted. A 0 cost
    /// would render a free-model badge on a paid model, which is worse than rendering no badge.
    /// </summary>
    [Fact]
    public void MapModelInfo_OmittedFieldsAreNull_NotDefaulted()
    {
        var map = ModelInfo.MapModelInfo(JsonNode.Parse(CostsPayload));

        // Present costs but no ceiling → the ceiling is null, not 0.
        Assert.Null(map["claude-haiku-4-5"]!["outputCostPerToken"]);
        Assert.Null(map["claude-haiku-4-5"]!["maxOutputTokens"]);

        // An empty model_info → every field null, and useCases an empty array rather than null.
        var bare = map["bare"]!;
        Assert.Null(bare["inputCostPerToken"]);
        Assert.Null(bare["outputCostPerToken"]);
        Assert.Null(bare["tier"]);
        Assert.Null(bare["maxOutputTokens"]);
        Assert.Empty(bare["useCases"]!.AsArray());
    }

    [Fact]
    public void MapModelInfo_SkipsNamelessEntries_AndToleratesMalformedPayloads()
    {
        Assert.Equal(3, ModelInfo.MapModelInfo(JsonNode.Parse(CostsPayload)).Count); // the nameless entry is skipped

        Assert.Empty(ModelInfo.MapModelInfo(null));
        Assert.Empty(ModelInfo.MapModelInfo(JsonNode.Parse("{}")));
        Assert.Empty(ModelInfo.MapModelInfo(JsonNode.Parse("{\"data\": \"nope\"}")));
        Assert.Empty(ModelInfo.MapModelInfo(JsonNode.Parse("{\"data\": []}")));
        // A non-object model_info must not throw — it reads as "no fields".
        Assert.Null(ModelInfo.MapModelInfo(JsonNode.Parse("{\"data\":[{\"model_name\":\"x\",\"model_info\":7}]}"))["x"]!["tier"]);
    }

    // ── FetchCeilingAsync (best-effort HTTP) ─────────────────────────────────────────────────────

    [Fact]
    public async Task FetchCeiling_ReturnsCeilingForKnownModel()
    {
        var http = ClientReturning(HttpStatusCode.OK, SamplePayload);

        Assert.Equal(8192, await ModelInfo.FetchCeilingAsync(http, "groq-compound"));
    }

    [Fact]
    public async Task FetchCeiling_UnknownModel_IsNull()
    {
        var http = ClientReturning(HttpStatusCode.OK, SamplePayload);

        Assert.Null(await ModelInfo.FetchCeilingAsync(http, "some-other-model"));
    }

    [Fact]
    public async Task FetchCeiling_GatewayErrorOrGarbage_IsNull()
    {
        Assert.Null(await ModelInfo.FetchCeilingAsync(ClientReturning(HttpStatusCode.InternalServerError, "boom"), "groq-compound"));
        Assert.Null(await ModelInfo.FetchCeilingAsync(ClientReturning(HttpStatusCode.OK, "not json"), "groq-compound"));
    }

    // ── Raised starvation defaults ───────────────────────────────────────────────────────────────

    [Fact]
    public void TurnLimits_RaisesStarvationDefaults()
    {
        Assert.Equal(8192, TurnLimits.DefaultMaxTokens);
        Assert.Equal(20, TurnLimits.DefaultMaxIterations);
        Assert.Equal(8192, TurnLimits.Default.MaxTokens);
        Assert.Equal(20, TurnLimits.Default.MaxIterations);
        Assert.Null(TurnLimits.Default.ModelMaxOutputTokens);
    }

    // ── TurnRunner threads the budget into the request (engine then clamps to the ceiling) ───────

    [Fact]
    public async Task TurnRunner_SendsConfiguredBudgetAsMaxTokens()
    {
        var chat = new MaxTokensCapturingClient();
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var runner = new TurnRunner(chat, store, limits: new TurnLimits(MaxTokens: 4096, MaxIterations: 20));

        await runner.RunAsync(session.ConversationId, "r1", "hello", _ => { });

        Assert.Equal(4096, chat.LastMaxOutputTokens);
    }

    [Fact]
    public async Task TurnRunner_DefaultLimits_SendRaisedBudget()
    {
        var chat = new MaxTokensCapturingClient();
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var runner = new TurnRunner(chat, store); // no limits ⇒ raised defaults

        await runner.RunAsync(session.ConversationId, "r1", "hello", _ => { });

        Assert.Equal(TurnLimits.DefaultMaxTokens, chat.LastMaxOutputTokens);
    }

    private static HttpClient ClientReturning(HttpStatusCode status, string body) =>
        new(new StubHandler(status, body)) { BaseAddress = new Uri("https://gateway.test/v1/") };

    private sealed class StubHandler : HttpMessageHandler
    {
        private readonly HttpStatusCode _status;
        private readonly string _body;

        public StubHandler(HttpStatusCode status, string body)
        {
            _status = status;
            _body = body;
        }

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            // The fetch must hit {gateway}/model/info (relative to the BaseAddress).
            Assert.Equal("https://gateway.test/v1/model/info", request.RequestUri!.ToString());
            return Task.FromResult(new HttpResponseMessage(_status) { Content = new StringContent(_body, Encoding.UTF8, "application/json") });
        }
    }

    /// <summary>Records the streaming request's <c>MaxOutputTokens</c> (the C# analog of the wire
    /// <c>max_tokens</c>) so a test can assert what the runner actually sent.</summary>
    private sealed class MaxTokensCapturingClient : IChatClient
    {
        public int? LastMaxOutputTokens { get; private set; }

        public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
        {
            LastMaxOutputTokens = options?.MaxOutputTokens;
            return Task.FromResult(new ChatResponse(new ChatMessage(ChatRole.Assistant, "ok")) { ModelId = "capture" });
        }

        public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
            IEnumerable<ChatMessage> messages, ChatOptions? options = null, [EnumeratorCancellation] CancellationToken cancellationToken = default)
        {
            LastMaxOutputTokens = options?.MaxOutputTokens;
            foreach (var update in new ChatResponse(new ChatMessage(ChatRole.Assistant, "ok")).ToChatResponseUpdates())
            {
                await Task.Yield();
                yield return update;
            }
        }

        public object? GetService(Type serviceType, object? serviceKey = null) => null;

        public void Dispose()
        {
        }
    }
}
