using System.Net.WebSockets;
using System.Text;
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
/// End-to-end proof of the <see cref="IToolHook"/> server seam: a host registers an
/// <c>IReadOnlyList&lt;IToolHook&gt;</c> in DI, and it flows through
/// <c>BuildDispatcher → FrameDispatcher → TurnRunner → SmoothAgent</c> onto every turn's tool
/// registry. The C# analog of the Rust server installing NarcHook on the operative's
/// <c>ToolRegistry</c>. A scripted <see cref="MockChatClient"/> keeps the turn offline (no gateway).
///
/// Asserts both hook phases over a REAL WebSocket: the spy sees pre+post for the tool call, and a
/// redacting <c>PostCallAsync</c> mutation reaches the wire — the streamed <c>toolResult</c> chunk
/// carries the scrubbed content, not the raw secret.
/// </summary>
public class ToolHookSeamTests
{
    private const string Tool = "lookup_secret";

    /// <summary>Records which tool names it saw on pre/post.</summary>
    private sealed class SpyHook : IToolHook
    {
        public List<string> PreCalls { get; } = new();
        public List<string> PostCalls { get; } = new();

        public Task PreCallAsync(FunctionCallContent call, CancellationToken cancellationToken = default)
        {
            lock (PreCalls)
            {
                PreCalls.Add(call.Name);
            }
            return Task.CompletedTask;
        }

        public Task PostCallAsync(FunctionCallContent call, FunctionResultContent result, CancellationToken cancellationToken = default)
        {
            lock (PostCalls)
            {
                PostCalls.Add(call.Name);
            }
            return Task.CompletedTask;
        }
    }

    /// <summary>Scrubs "secret" out of the tool result in place (the redaction seam).</summary>
    private sealed class RedactHook : IToolHook
    {
        public Task PostCallAsync(FunctionCallContent call, FunctionResultContent result, CancellationToken cancellationToken = default)
        {
            var text = result.Result?.ToString() ?? string.Empty;
            result.Result = text.Replace("secret", "[REDACTED]", StringComparison.Ordinal);
            return Task.CompletedTask;
        }
    }

    private static WebApplication BuildApp(SpyHook spy)
    {
        var chat = new MockChatClient();
        chat.PushToolCall("call-1", Tool, new Dictionary<string, object?>());
        chat.PushText("All set.");

        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton<IChatClient>(chat);
        builder.Services.AddSingleton<IReadOnlyList<AITool>>(new AITool[]
        {
            AIFunctionFactory.Create(() => "the secret token is 12345", Tool, "Return a value that contains a secret."),
        });
        // The host-supplied hook chain: spy first (observe), redactor second (mutate). This is the
        // exact seam under test — BuildDispatcher resolves IReadOnlyList<IToolHook> from DI.
        builder.Services.AddSingleton<IReadOnlyList<IToolHook>>(new IToolHook[] { spy, new RedactHook() });
        builder.Services.AddSmoothOperatorServer();

        var app = builder.Build();
        app.MapSmoothOperatorWebSocket("/ws");
        return app;
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
            var type = ev["type"]?.GetValue<string>();
            if (type is not ("keepalive" or "pong"))
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
            ["agentId"] = "11111111-1111-1111-1111-111111111111",
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

    [Fact]
    public async Task RegisteredToolHooks_FirePrePost_AndPostCallRedactionReachesTheWire()
    {
        var spy = new SpyHook();
        await using var app = BuildApp(spy);
        await app.StartAsync();
        var server = app.GetTestServer();
        using var socket = await server.CreateWebSocketClient().ConnectAsync(new Uri(server.BaseAddress, "ws"), CancellationToken.None);
        var sessionId = await CreateSessionAsync(socket);

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["message"] = "look it up",
        });

        var toolResults = new List<JsonObject>();
        while (true)
        {
            var ev = await NextEventAsync(socket);
            var type = ev["type"]!.GetValue<string>();
            if (type == "stream_chunk")
            {
                var tr = ev["data"]?["state"]?["rawResponse"]?["toolResult"]?.AsObject();
                if (tr is not null)
                {
                    toolResults.Add(tr);
                }
            }
            else if (type == "eventual_response")
            {
                Assert.Equal(200, ev["status"]!.GetValue<int>());
                break;
            }
        }

        // The hook fired on both phases for the tool call (proves the DI → dispatcher → runner →
        // engine seam wired the host's hooks onto the per-turn registry).
        Assert.Contains(Tool, spy.PreCalls);
        Assert.Contains(Tool, spy.PostCalls);

        // The redacting PostCallAsync mutation reached the wire: the streamed toolResult is scrubbed.
        var resultText = Assert.Single(toolResults)["result"]!.GetValue<string>();
        Assert.Contains("[REDACTED]", resultText, StringComparison.Ordinal);
        Assert.DoesNotContain("secret", resultText, StringComparison.Ordinal);

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }
}
