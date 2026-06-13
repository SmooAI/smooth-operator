using System.Net.WebSockets;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.TestHost;
using Microsoft.Extensions.AI;
using Microsoft.Extensions.DependencyInjection;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server.AspNetCore;

namespace SmooAI.SmoothOperator.Server.IntegrationTests;

/// <summary>
/// End-to-end integration tests: boot the ASP.NET Core WebSocket host in-process and drive the
/// wire protocol over a REAL WebSocket — the C# parity of the Rust server's
/// <c>tests/protocol_smoke.rs</c>. CI-safe (a scripted mock IChatClient, no gateway).
/// </summary>
public class WebSocketProtocolIntegrationTests
{
    private static WebApplication BuildApp(IChatClient chat)
    {
        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton(chat);
        builder.Services.AddSmoothOperatorServer();

        var app = builder.Build();
        app.MapSmoothOperatorWebSocket("/ws");
        return app;
    }

    private static WebApplication BuildAppWithAcl(IChatClient chat, AclKnowledgeStore knowledge, AuthMode mode)
    {
        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton(chat);
        builder.Services.AddSingleton<IAccessKnowledge>(knowledge);
        builder.Services.AddSingleton(new TokenAccessResolver(new AuthOptions { Mode = mode }));
        builder.Services.AddSmoothOperatorServer();

        var app = builder.Build();
        app.MapSmoothOperatorWebSocket("/ws");
        return app;
    }

    private static WebApplication BuildAppWithReranker(IChatClient chat, AclKnowledgeStore knowledge, IReranker? reranker)
    {
        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton(chat);
        builder.Services.AddSingleton<IAccessKnowledge>(knowledge);
        if (reranker is not null)
        {
            builder.Services.AddSingleton(reranker);
        }
        builder.Services.AddSmoothOperatorServer();

        var app = builder.Build();
        app.MapSmoothOperatorWebSocket("/ws");
        return app;
    }

    [Fact]
    public async Task Reranker_ReordersCitations_OverWebSocket()
    {
        // Three public docs all matching the query, so three citations come back. A reranker that
        // reverses the candidate order proves the dispatcher→runner→reranker wiring actually applies
        // the reranker on the live chat path: the reranked citation order is the reverse of the
        // un-reranked order (same set, different order).
        static AclKnowledgeStore Corpus()
        {
            var kb = new AclKnowledgeStore();
            kb.IngestAsync(new KnowledgeDocument("a", "Refund policy details for orders.", "a.md"), DocumentAcl.PublicAcl).GetAwaiter().GetResult();
            kb.IngestAsync(new KnowledgeDocument("b", "Shipping policy and delivery windows.", "b.md"), DocumentAcl.PublicAcl).GetAwaiter().GetResult();
            kb.IngestAsync(new KnowledgeDocument("c", "Privacy policy and data handling.", "c.md"), DocumentAcl.PublicAcl).GetAwaiter().GetResult();
            return kb;
        }

        List<string> baseline;
        await using (var app = BuildAppWithReranker(new MockChatClient().PushText("ok"), Corpus(), reranker: null))
        {
            await app.StartAsync();
            baseline = await CitationSourcesAsync(app.GetTestServer(), token: null, "policy");
            await app.StopAsync();
        }

        List<string> reranked;
        await using (var app = BuildAppWithReranker(new MockChatClient().PushText("ok"), Corpus(), new ReversingReranker()))
        {
            await app.StartAsync();
            reranked = await CitationSourcesAsync(app.GetTestServer(), token: null, "policy");
            await app.StopAsync();
        }

        Assert.Equal(3, baseline.Count);
        Assert.Equal(baseline.AsEnumerable().Reverse(), reranked);          // the reranker visibly reordered the citation path
        Assert.Equal(baseline.OrderBy(s => s), reranked.OrderBy(s => s));   // and it's the same set of sources
    }

    /// <summary>A test reranker that reverses the candidate order — a visible, deterministic reorder
    /// that proves the reranker is invoked in the pipeline (vs the no-op default).</summary>
    private sealed class ReversingReranker : IReranker
    {
        public Task<IReadOnlyList<KnowledgeResult>> RerankAsync(string query, IReadOnlyList<KnowledgeResult> candidates, int topK, CancellationToken cancellationToken = default) =>
            Task.FromResult<IReadOnlyList<KnowledgeResult>>(candidates.Reverse().Take(topK).ToArray());
    }

    [Fact]
    public async Task Acl_PrivateDoc_OnlyReachesEntitledUser_OverWebSocket()
    {
        var kb = new AclKnowledgeStore();
        await kb.IngestAsync(new KnowledgeDocument("pub", "Support hours are 9 to 5.", "public.md"), DocumentAcl.PublicAcl);
        await kb.IngestAsync(
            new KnowledgeDocument("secret", "The private launch code is hunter2.", "acme/private/launch.md"),
            DocumentAcl.ForGroups("github:acme/private"));

        await using var app = BuildAppWithAcl(new MockChatClient().PushText("Here is what I found."), kb, AuthMode.Trusted);
        await app.StartAsync();
        var server = app.GetTestServer();

        // The entitled user (token carries the group) sees the private doc among the citations.
        var entitledToken = TrustedToken(new { sub = "u1", org = "acme", role = "basic", groups = new[] { "github:acme/private" } });
        var entitled = await CitationSourcesAsync(server, entitledToken, "private launch code");
        Assert.Contains("acme/private/launch.md", entitled);

        // The anonymous user (no token) must NOT — the chat path enforces ACL end-to-end.
        var anonymous = await CitationSourcesAsync(server, token: null, "private launch code");
        Assert.DoesNotContain("acme/private/launch.md", anonymous);

        await app.StopAsync();
    }

    private static async Task<List<string>> CitationSourcesAsync(TestServer server, string? token, string message)
    {
        var path = token is null ? "ws" : $"ws?token={token}";
        using var socket = await server.CreateWebSocketClient().ConnectAsync(new Uri(server.BaseAddress, path), CancellationToken.None);

        await SendAsync(socket, """{"action":"create_conversation_session","requestId":"cs"}""");
        var sessionId = (await ReceiveAsync(socket))["data"]!["sessionId"]!.GetValue<string>();

        await SendAsync(socket, $$"""{"action":"send_message","requestId":"sm","sessionId":"{{sessionId}}","message":"{{message}}"}""");
        JsonObject terminal;
        do
        {
            terminal = await ReceiveAsync(socket);
        }
        while (terminal["type"]!.GetValue<string>() != "eventual_response");

        var sources = new List<string>();
        if (terminal["data"]!["data"]!["citations"] is JsonArray citations)
        {
            foreach (var citation in citations)
            {
                sources.Add(citation!["title"]!.GetValue<string>());
            }
        }

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        return sources;
    }

    private static string TrustedToken(object claims) =>
        Convert.ToBase64String(Encoding.UTF8.GetBytes(JsonSerializer.Serialize(claims))).TrimEnd('=').Replace('+', '-').Replace('/', '_');

    private static Task SendAsync(WebSocket socket, string json) =>
        socket.SendAsync(Encoding.UTF8.GetBytes(json), WebSocketMessageType.Text, endOfMessage: true, CancellationToken.None);

    private static async Task<JsonObject> ReceiveAsync(WebSocket socket)
    {
        var buffer = new byte[16 * 1024];
        using var stream = new MemoryStream();
        WebSocketReceiveResult result;
        do
        {
            result = await socket.ReceiveAsync(buffer, CancellationToken.None);
            stream.Write(buffer, 0, result.Count);
        }
        while (!result.EndOfMessage);
        return JsonNode.Parse(Encoding.UTF8.GetString(stream.ToArray()))!.AsObject();
    }

    private static async Task<WebSocket> ConnectAsync(TestServer server)
    {
        var client = server.CreateWebSocketClient();
        return await client.ConnectAsync(new Uri(server.BaseAddress, "ws"), CancellationToken.None);
    }

    [Fact]
    public async Task FullConversation_OverRealWebSocket()
    {
        await using var app = BuildApp(new MockChatClient().PushText("Your return window is 17 days."));
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());

        // 1. ping → pong (mirrors protocol_smoke ping_returns_pong)
        await SendAsync(socket, """{"action":"ping","requestId":"ping-1"}""");
        var pong = await ReceiveAsync(socket);
        Assert.Equal("pong", pong["type"]!.GetValue<string>());
        Assert.Equal("ping-1", pong["requestId"]!.GetValue<string>());

        // 2. create_conversation_session → descriptor (mirrors create_session_returns_valid_descriptor)
        var agentId = Guid.NewGuid().ToString();
        await SendAsync(socket, $$"""{"action":"create_conversation_session","requestId":"cs-1","agentId":"{{agentId}}","userName":"Test"}""");
        var created = await ReceiveAsync(socket);
        Assert.Equal("immediate_response", created["type"]!.GetValue<string>());
        Assert.Equal(200, created["status"]!.GetValue<int>());
        var sessionId = created["data"]!["sessionId"]!.GetValue<string>();
        Assert.True(Guid.TryParse(sessionId, out _), "sessionId must be a UUID");
        Assert.True(Guid.TryParse(created["data"]!["conversationId"]!.GetValue<string>(), out _));
        Assert.Equal(agentId, created["data"]!["agentId"]!.GetValue<string>()); // echoed back

        // 3. send_message → 202 ack → stream_token(s) → eventual_response (the happy path the mock enables)
        await SendAsync(socket, $$"""{"action":"send_message","requestId":"sm-1","sessionId":"{{sessionId}}","message":"How long can I return?"}""");
        var ack = await ReceiveAsync(socket);
        Assert.Equal("immediate_response", ack["type"]!.GetValue<string>());
        Assert.Equal(202, ack["status"]!.GetValue<int>());

        var sawToken = false;
        JsonObject ev;
        do
        {
            ev = await ReceiveAsync(socket);
            if (ev["type"]!.GetValue<string>() == "stream_token")
            {
                sawToken = true;
            }
        }
        while (ev["type"]!.GetValue<string>() != "eventual_response");

        Assert.True(sawToken, "expected at least one stream_token before the terminal event");
        var parts = ev["data"]!["data"]!["response"]!["responseParts"]!.AsArray();
        Assert.Contains(parts, p => p!.GetValue<string>().Contains("17 days"));

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task UnknownAction_ErrorsWithoutDroppingConnection()
    {
        await using var app = BuildApp(new MockChatClient());
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());

        await SendAsync(socket, """{"action":"frobnicate","requestId":"x1"}""");
        var error = await ReceiveAsync(socket);
        Assert.Equal("error", error["type"]!.GetValue<string>());

        // The connection survives — a subsequent ping still works (mirrors
        // unknown_action_errors_without_dropping_connection).
        await SendAsync(socket, """{"action":"ping","requestId":"ping-2"}""");
        var pong = await ReceiveAsync(socket);
        Assert.Equal("pong", pong["type"]!.GetValue<string>());
        Assert.Equal("ping-2", pong["requestId"]!.GetValue<string>());

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }
}
