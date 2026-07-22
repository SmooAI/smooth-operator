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
/// Per-agent write-confirmation (HITL) patterns: a host that registers an <see cref="IAgentConfigResolver"/>
/// with <see cref="AgentConfig.ConfirmToolPatterns"/> gates writes PER AGENT rather than sharing the one
/// global <see cref="ConfirmTools"/> singleton. Driven end-to-end over a REAL WebSocket against the
/// in-process host (scripted <see cref="MockChatClient"/>, so the gated tool call is deterministic).
///
/// Covers the three behaviours of the compose rule: per-agent patterns are honored (no global needed),
/// the global is the fallback when the agent doesn't specify, and a per-agent list OVERRIDES the global
/// (both an override that adds gating the global lacked, and an explicit empty list that removes gating
/// the global would have applied).
/// </summary>
public class PerAgentConfirmToolsTests
{
    private const string AgentId = "11111111-1111-1111-1111-111111111111";
    private const string GatedTool = "delete_record";

    private static WebApplication BuildApp(AgentConfig? agentConfig, ConfirmTools? global)
    {
        var chat = new MockChatClient();
        chat.PushToolCall("call-1", GatedTool, new Dictionary<string, object?> { ["id"] = "42" });
        chat.PushText("Done — record 42 was deleted.");

        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton<IChatClient>(chat);
        builder.Services.AddSingleton<IReadOnlyList<AITool>>(new AITool[]
        {
            AIFunctionFactory.Create(() => "Record 42 deleted.", GatedTool, "Delete a record by id (a state-mutating write)."),
        });
        if (agentConfig is not null)
        {
            builder.Services.AddSingleton<IAgentConfigResolver>(new StaticAgentConfigResolver().Set(AgentId, agentConfig));
        }
        if (global is not null)
        {
            builder.Services.AddSingleton(global);
        }
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

    private static async Task<string> CreateSessionAsync(WebSocket socket)
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
                return ev["data"]!["sessionId"]!.GetValue<string>();
            }
        }
    }

    /// <summary>
    /// Send a message that triggers the gated tool and report whether the turn PARKED on a
    /// write-confirmation. Gated ⇒ approve the confirmation and drain to completion; not-gated ⇒ the
    /// tool ran inline. Either way the turn reaches eventual_response so teardown is clean.
    /// </summary>
    private static async Task<bool> SendAndDetectGatingAsync(WebSocket socket, string sessionId)
    {
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["message"] = "delete record 42",
        });

        var gated = false;
        while (true)
        {
            var ev = await NextEventAsync(socket);
            switch (ev["type"]!.GetValue<string>())
            {
                case "write_confirmation_required":
                    gated = true;
                    // Approve so the turn resumes and completes rather than parking until teardown.
                    await SendAsync(socket, new JsonObject
                    {
                        ["action"] = "confirm_tool_action",
                        ["requestId"] = "r-confirm",
                        ["sessionId"] = sessionId,
                        ["approved"] = true,
                    });
                    break;
                case "eventual_response":
                    return gated;
            }
        }
    }

    private static async Task<bool> RunAsync(AgentConfig? agentConfig, ConfirmTools? global)
    {
        await using var app = BuildApp(agentConfig, global);
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket);
        var gated = await SendAndDetectGatingAsync(socket, sessionId);
        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
        return gated;
    }

    [Fact]
    public async Task PerAgentPatterns_GateTool_WithNoGlobalConfigured()
    {
        // Only the per-agent patterns exist (no global ConfirmTools). The tool is gated for this agent.
        var config = new AgentConfig(ConfirmToolPatterns: new[] { GatedTool });
        Assert.True(await RunAsync(config, global: null));
    }

    [Fact]
    public async Task GlobalPatterns_AreTheFallback_WhenAgentDoesNotSpecify()
    {
        // Agent config present but carries NO ConfirmToolPatterns (null) → fall back to the global.
        var config = new AgentConfig(Visibility: "internal");
        Assert.Null(config.ConfirmToolPatterns);
        Assert.True(await RunAsync(config, new ConfirmTools(GatedTool)));
    }

    [Fact]
    public async Task NoAgentConfigResolver_StillUsesGlobal()
    {
        // Backward compatibility: no resolver registered at all → the global singleton still gates.
        Assert.True(await RunAsync(agentConfig: null, new ConfirmTools(GatedTool)));
    }

    [Fact]
    public async Task PerAgentPatterns_Override_Global_AddingGating()
    {
        // The global wouldn't match this tool; the per-agent list does → per-agent wins, tool is gated.
        var config = new AgentConfig(ConfirmToolPatterns: new[] { GatedTool });
        Assert.True(await RunAsync(config, new ConfirmTools("some_other_tool")));
    }

    [Fact]
    public async Task EmptyPerAgentPatterns_Override_Global_RemovingGating()
    {
        // The global WOULD gate the tool; the agent's explicit empty list overrides it → NOT gated.
        var config = new AgentConfig(ConfirmToolPatterns: Array.Empty<string>());
        Assert.False(await RunAsync(config, new ConfirmTools(GatedTool)));
    }
}
