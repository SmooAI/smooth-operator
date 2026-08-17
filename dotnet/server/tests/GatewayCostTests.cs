using System.Net;
using System.Text;
using System.Text.Json.Nodes;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// The gateway's per-request cost reaches <c>TurnResult.Usage.CostUsd</c>, and so
/// <c>eventual_response.usage.costUsd</c>.
///
/// Cost is reported ONLY in a response header. The host used to inject the MEAI OpenAI adapter,
/// whose parsed <c>ChatResponse</c> drops HTTP headers — so the cost could not be read at all, and
/// this runner hardcoded <c>new TurnUsage(0, …)</c>. These drive a REAL turn against a REAL local
/// gateway (<see cref="HttpListener"/> speaking SSE), so they fail if the host ever goes back to
/// injecting a header-dropping client, or if the runner stops folding the cost.
/// </summary>
public class GatewayCostTests : IDisposable
{
    private readonly HttpListener _listener = new();
    private readonly string _baseUrl;
    private Task? _serving;

    public GatewayCostTests()
    {
        var port = GetFreePort();
        _listener.Prefixes.Add($"http://127.0.0.1:{port}/");
        _listener.Start();
        _baseUrl = $"http://127.0.0.1:{port}/v1";
    }

    public void Dispose()
    {
        _listener.Close();
        GC.SuppressFinalize(this);
    }

    /// <summary>Serve one SSE completion, stamped with <paramref name="headers"/>.</summary>
    private void Serve(Dictionary<string, string> headers)
    {
        _serving = Task.Run(async () =>
        {
            var ctx = await _listener.GetContextAsync();
            using (var reader = new StreamReader(ctx.Request.InputStream, Encoding.UTF8))
            {
                await reader.ReadToEndAsync();
            }
            foreach (var (name, value) in headers)
            {
                ctx.Response.AddHeader(name, value);
            }
            ctx.Response.ContentType = "text/event-stream";
            await WriteSseAsync(ctx.Response, """{"choices":[{"index":0,"delta":{"content":"Seventeen days."}}]}""");
            await WriteSseAsync(ctx.Response, """{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}""");
            await ctx.Response.OutputStream.WriteAsync(Encoding.UTF8.GetBytes("data: [DONE]\n\n"));
            ctx.Response.Close();
        });
    }

    private static async Task WriteSseAsync(HttpListenerResponse response, string payload)
    {
        await response.OutputStream.WriteAsync(Encoding.UTF8.GetBytes($"data: {payload}\n\n"));
        await response.OutputStream.FlushAsync();
    }

    private static int GetFreePort()
    {
        var l = new System.Net.Sockets.TcpListener(IPAddress.Loopback, 0);
        l.Start();
        var port = ((IPEndPoint)l.LocalEndpoint).Port;
        l.Stop();
        return port;
    }

    /// <summary>Run one real turn through the server's runner and return its usage.</summary>
    private async Task<TurnUsage?> TurnUsageAsync(Dictionary<string, string> headers)
    {
        Serve(headers);
        using var client = new GatewayChatClient(_baseUrl, "k", "m");
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var runner = new TurnRunner(client, store);

        var result = await runner.RunAsync(session.ConversationId, "r1", "How long can I return?", _ => { });
        await _serving!;
        return result.Usage;
    }

    [Fact]
    public async Task HeaderCostReachesTheTurnUsage()
    {
        var usage = await TurnUsageAsync(new() { ["x-litellm-response-cost-margin-amount"] = "0.25" });

        Assert.NotNull(usage);
        Assert.Equal(0.25, usage!.CostUsd);
        // Token counts still come from the stream's usage chunk, unaffected.
        Assert.Equal(10, usage.PromptTokens);
        Assert.Equal(5, usage.CompletionTokens);
    }

    [Fact]
    public async Task ZeroMarginDoesNotZeroRealSpend()
    {
        var usage = await TurnUsageAsync(new()
        {
            ["x-litellm-response-cost-margin-amount"] = "0",
            ["x-litellm-response-cost-original"] = "0.5",
        });

        Assert.NotNull(usage);
        Assert.Equal(0.5, usage!.CostUsd);
    }

    [Fact]
    public async Task AnAbsentHeaderLeavesTheCostUnmeasured()
    {
        // No local pricing table is wired into this server, so an unpriced turn reports 0 —
        // but it must never report one of the gateway's numbers from a previous turn.
        var usage = await TurnUsageAsync(new());

        Assert.NotNull(usage); // token counts still recorded
        Assert.Equal(0, usage!.CostUsd);
        Assert.Equal(10, usage.PromptTokens);
    }

    [Fact]
    public async Task EveryCandidateReportingZeroIsIdenticalToAbsent()
    {
        // The distinction the whole cost fix rests on: a PRESENT zero is not locked in as a
        // real $0 — it falls through exactly as an absent header does.
        var allZero = await TurnUsageAsync(new()
        {
            ["x-litellm-response-cost"] = "0",
            ["x-cost-usd"] = "0",
        });

        Assert.NotNull(allZero);
        Assert.Equal(0, allZero!.CostUsd);
    }
}
