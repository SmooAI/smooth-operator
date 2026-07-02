using System.Net.WebSockets;
using System.Text;
using System.Text.Json.Nodes;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.TestHost;
using Microsoft.Extensions.AI;
using Microsoft.Extensions.DependencyInjection;
using SmooAI.SmoothOperator.Server.AspNetCore;

namespace SmooAI.SmoothOperator.Server.IntegrationTests;

/// <summary>
/// Per-agent config enforced end-to-end over a REAL WebSocket against the in-process host: authLevel
/// gating at tool-execution time (admin-on-public blocked; end_user needs a verified session; internal
/// auto-satisfied; supportsAuthRequirement opt-in), per-tool config delivery to the executing tool, and
/// judge-advanced workflow across a multi-turn session. The turn runs offline (scripted MockChatClient),
/// so the tool-call path is deterministic. Mirrors the monorepo tool-execution + registry semantics.
/// </summary>
public class AgentConfigIntegrationTests
{
    private const string AgentId = "11111111-1111-1111-1111-111111111111";

    private sealed record Session(string SessionId, string ConversationId);

    private sealed class StubAuthenticator : ISessionAuthenticator
    {
        private readonly bool _authed;

        public StubAuthenticator(bool authed) => _authed = authed;

        public Task<bool> IsAuthenticatedAsync(string conversationId, CancellationToken cancellationToken = default) => Task.FromResult(_authed);
    }

    private sealed class StubJudge : IWorkflowJudge
    {
        private readonly WorkflowVerdict _verdict;

        public StubJudge(WorkflowVerdict verdict) => _verdict = verdict;

        public Task<WorkflowVerdict> JudgeAsync(ConversationWorkflow workflow, ConversationWorkflowStep step, string userMessage, string agentReply, CancellationToken cancellationToken = default) =>
            Task.FromResult(_verdict);
    }

    private static AITool AuthTool(string name, string result) =>
        AIFunctionFactory.Create(() => result, new AIFunctionFactoryOptions
        {
            Name = name,
            Description = $"{name} (declares auth support)",
            AdditionalProperties = new Dictionary<string, object?> { ["supportsAuthRequirement"] = true },
        });

    private static WebApplication BuildApp(
        AgentConfig config,
        IReadOnlyList<AITool> tools,
        string toolToCall,
        ISessionAuthenticator? authenticator = null,
        IWorkflowJudge? judge = null,
        Action<IServiceCollection>? extra = null)
    {
        var chat = new MockChatClient();
        if (!string.IsNullOrEmpty(toolToCall))
        {
            chat.PushToolCall("call-1", toolToCall, new Dictionary<string, object?>());
        }
        chat.PushText("All done.");

        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton<IChatClient>(chat);
        builder.Services.AddSingleton(tools);
        builder.Services.AddSingleton<IAgentConfigResolver>(new StaticAgentConfigResolver().Set(AgentId, config));
        if (authenticator is not null)
        {
            builder.Services.AddSingleton(authenticator);
        }
        if (judge is not null)
        {
            builder.Services.AddSingleton(judge);
        }
        extra?.Invoke(builder.Services);
        builder.Services.AddSmoothOperatorServer();

        var app = builder.Build();
        app.MapSmoothOperatorWebSocket("/ws");
        return app;
    }

    private static async Task<WebSocket> ConnectAsync(TestServer server)
    {
        var client = server.CreateWebSocketClient();
        return await client.ConnectAsync(new Uri(server.BaseAddress, "ws"), CancellationToken.None);
    }

    private static Task SendAsync(WebSocket socket, JsonObject frame) =>
        socket.SendAsync(Encoding.UTF8.GetBytes(frame.ToJsonString()), WebSocketMessageType.Text, endOfMessage: true, CancellationToken.None);

    private static async Task<JsonObject> NextEventAsync(WebSocket socket)
    {
        while (true)
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

            var ev = JsonNode.Parse(Encoding.UTF8.GetString(stream.ToArray()))!.AsObject();
            if (ev["type"]?.GetValue<string>() is not ("keepalive" or "pong"))
            {
                return ev;
            }
        }
    }

    private static async Task<Session> CreateSessionAsync(WebSocket socket)
    {
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "create_conversation_session",
            ["requestId"] = "r-create",
            ["agentId"] = AgentId,
            ["userName"] = "Alice",
            ["userEmail"] = "alice@example.com",
        });
        while (true)
        {
            var ev = await NextEventAsync(socket);
            if (ev["type"]!.GetValue<string>() == "immediate_response")
            {
                return new Session(ev["data"]!["sessionId"]!.GetValue<string>(), ev["data"]!["conversationId"]!.GetValue<string>());
            }
        }
    }

    /// <summary>Send one message and collect the tool-result strings + the final reply.</summary>
    private static async Task<(List<string> ToolResults, string Reply)> SendMessageAsync(WebSocket socket, string sessionId, string message)
    {
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["message"] = message,
        });

        var toolResults = new List<string>();
        while (true)
        {
            var ev = await NextEventAsync(socket);
            switch (ev["type"]!.GetValue<string>())
            {
                case "stream_chunk":
                    var tr = ev["data"]?["state"]?["rawResponse"]?["toolResult"]?.AsObject();
                    if (tr is not null)
                    {
                        toolResults.Add(tr["result"]!.GetValue<string>());
                    }
                    break;
                case "eventual_response":
                    var reply = ev["data"]!["data"]!["response"]!["responseParts"]![0]!.GetValue<string>();
                    return (toolResults, reply);
            }
        }
    }

    private static AgentConfig WithTool(string toolId, string authLevel, string visibility, JsonObject? config = null) =>
        new(EnabledTools: new[] { new EnabledTool(toolId, true, authLevel, config) }, Visibility: visibility);

    private static async Task<List<string>> RunSingleTurn(WebApplication app, string message = "go")
    {
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var session = await CreateSessionAsync(socket);
        var (toolResults, _) = await SendMessageAsync(socket, session.SessionId, message);
        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
        return toolResults;
    }

    [Fact]
    public async Task AdminTool_OnPublicAgent_IsBlocked()
    {
        await using var app = BuildApp(WithTool("admin_tool", "admin", "public"), new[] { AuthTool("admin_tool", "REAL_ADMIN_RESULT") }, "admin_tool");
        var toolResults = await RunSingleTurn(app);
        Assert.Contains(toolResults, r => r.Contains("requires admin authentication", StringComparison.Ordinal));
        Assert.DoesNotContain(toolResults, r => r.Contains("REAL_ADMIN_RESULT", StringComparison.Ordinal));
    }

    [Fact]
    public async Task EndUserTool_PublicAgent_Unauthenticated_IsBlocked()
    {
        // Default (no authenticator registered) fails closed; an explicit false authenticator is the same.
        await using var app = BuildApp(WithTool("user_tool", "end_user", "public"), new[] { AuthTool("user_tool", "REAL_USER_RESULT") }, "user_tool",
            authenticator: new StubAuthenticator(false));
        var toolResults = await RunSingleTurn(app);
        Assert.Contains(toolResults, r => r.Contains("verify your identity", StringComparison.Ordinal));
        Assert.DoesNotContain(toolResults, r => r.Contains("REAL_USER_RESULT", StringComparison.Ordinal));
    }

    [Fact]
    public async Task EndUserTool_PublicAgent_Authenticated_Executes()
    {
        await using var app = BuildApp(WithTool("user_tool", "end_user", "public"), new[] { AuthTool("user_tool", "REAL_USER_RESULT") }, "user_tool",
            authenticator: new StubAuthenticator(true));
        var toolResults = await RunSingleTurn(app);
        Assert.Contains(toolResults, r => r.Contains("REAL_USER_RESULT", StringComparison.Ordinal));
    }

    [Fact]
    public async Task InternalAgent_AdminTool_AutoSatisfied_Executes()
    {
        // Internal agent, no authenticator: admin (and end_user) auto-satisfied by the session.
        await using var app = BuildApp(WithTool("admin_tool", "admin", "internal"), new[] { AuthTool("admin_tool", "REAL_ADMIN_RESULT") }, "admin_tool");
        var toolResults = await RunSingleTurn(app);
        Assert.Contains(toolResults, r => r.Contains("REAL_ADMIN_RESULT", StringComparison.Ordinal));
    }

    [Fact]
    public async Task Tool_WithoutSupportsAuthRequirement_IsNotGated()
    {
        // authLevel set, but the tool didn't opt in → not gated → runs (faithful to the reference).
        var plainTool = AIFunctionFactory.Create(() => "REAL_PLAIN_RESULT", "plain_tool", "no auth support");
        await using var app = BuildApp(WithTool("plain_tool", "admin", "public"), new[] { plainTool }, "plain_tool");
        var toolResults = await RunSingleTurn(app);
        Assert.Contains(toolResults, r => r.Contains("REAL_PLAIN_RESULT", StringComparison.Ordinal));
    }

    [Fact]
    public async Task PerToolConfig_IsDeliveredToTheExecutingTool()
    {
        // The tool reads its per-tool config from the invocation context and returns it — proving delivery.
        var lookup = AIFunctionFactory.Create((AIFunctionArguments a) =>
        {
            var cfg = a.Context is not null && a.Context.TryGetValue(ToolAuthGate.ToolConfigKey, out var v) ? v as JsonObject : null;
            return cfg?["region"]?.GetValue<string>() ?? "NO_CONFIG";
        }, "lookup", "reads its per-tool config");

        var config = WithTool("lookup", "none", "public", new JsonObject { ["region"] = "us-east" });
        await using var app = BuildApp(config, new[] { lookup }, "lookup");
        var toolResults = await RunSingleTurn(app);
        Assert.Contains(toolResults, r => r == "us-east");
    }

    [Fact]
    public async Task Workflow_AdvancesAcrossAMultiTurnSession()
    {
        var workflow = new ConversationWorkflow("Book a demo", new[]
        {
            new ConversationWorkflowStep("greet", "Greet", "Said hi", "qualify"),
            new ConversationWorkflowStep("qualify", "Qualify", "Got budget", null),
        });
        var store = new InMemorySessionStore();
        await using var app = BuildApp(
            new AgentConfig(Workflow: workflow),
            Array.Empty<AITool>(),
            toolToCall: "", // text-only turn
            judge: new StubJudge(WorkflowVerdict.Yes),
            extra: s => s.AddSingleton<ISessionStore>(store));

        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var session = await CreateSessionAsync(socket);

        // Turn starts on the first step (greet); the Yes judge advances it to greet.next = qualify,
        // persisted per conversation.
        await SendMessageAsync(socket, session.SessionId, "hello");
        Assert.Equal("qualify", await store.GetWorkflowStepAsync(session.ConversationId));

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }
}
