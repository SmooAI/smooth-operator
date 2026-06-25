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
/// Write-confirmation HITL — the pause → <c>confirm_tool_action</c> → resume path, driven end-to-end
/// over a REAL WebSocket against the in-process ASP.NET Core host. The C# parity of the Python
/// <c>test_confirm_tool_action.py</c> and the Rust <c>tests/confirm_tool_action.rs</c>.
///
/// The turn runs offline (a scripted <see cref="MockChatClient"/> calls the gated tool), so there is
/// no gateway / flakiness. The <c>confirm_tool_action</c> frame arrives on the same connection's
/// reader while the turn is parked — proving the turn runs as a background task (not awaited inline),
/// so the reader stays free to receive the confirmation. Covers approve, reject, and the fail-closed
/// validation (<c>NO_PENDING_CONFIRMATION</c> / <c>VALIDATION_ERROR</c>).
/// </summary>
public class ConfirmToolActionTests
{
    private const string GatedTool = "delete_record";

    private static WebApplication BuildApp()
    {
        var chat = new MockChatClient();
        chat.PushToolCall("call-1", GatedTool, new Dictionary<string, object?> { ["id"] = "42" });
        chat.PushText("Done — record 42 was deleted.");

        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton<IChatClient>(chat);
        // The gated tool returns a fixed result so the approved path is deterministic.
        builder.Services.AddSingleton<IReadOnlyList<AITool>>(new AITool[]
        {
            AIFunctionFactory.Create(() => "Record 42 deleted.", GatedTool, "Delete a record by id (a state-mutating write)."),
        });
        builder.Services.AddSingleton(new ConfirmTools(GatedTool));
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
    public async Task ApprovedConfirmation_RunsTheGatedTool_AndCompletes()
    {
        await using var app = BuildApp();
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket);

        // send_message: the gated tool call parks the turn → 202 ack, then the
        // write_confirmation_required prompt (and the deferred toolCall chunk).
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["message"] = "delete record 42",
        });

        var ack = await NextEventAsync(socket);
        Assert.Equal("immediate_response", ack["type"]!.GetValue<string>());
        Assert.Equal(202, ack["status"]!.GetValue<int>());

        var confirm = await NextEventAsync(socket);
        Assert.Equal("write_confirmation_required", confirm["type"]!.GetValue<string>());
        Assert.Equal("r-msg", confirm["requestId"]!.GetValue<string>());
        Assert.Equal(GatedTool, confirm["data"]!["data"]!["toolId"]!.GetValue<string>());
        Assert.False(string.IsNullOrEmpty(confirm["data"]!["data"]!["actionDescription"]!.GetValue<string>()));

        // The deferred toolCall chunk arrives right after the prompt (canonical order).
        var toolCallChunk = await NextEventAsync(socket);
        Assert.Equal("stream_chunk", toolCallChunk["type"]!.GetValue<string>());
        Assert.Equal(GatedTool, toolCallChunk["data"]!["state"]!["rawResponse"]!["toolCall"]!["name"]!.GetValue<string>());

        // Confirm: approve. The reader was free to receive THIS frame while the turn was parked.
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "confirm_tool_action",
            ["requestId"] = "r-confirm",
            ["sessionId"] = sessionId,
            ["approved"] = true,
        });

        var tokens = new StringBuilder();
        var toolResults = new List<JsonObject>();
        var sawAck = false;
        while (true)
        {
            var ev = await NextEventAsync(socket);
            var type = ev["type"]!.GetValue<string>();
            if (type == "immediate_response" && ev["status"]!.GetValue<int>() == 200)
            {
                sawAck = true;
                Assert.True(ev["data"]!["approved"]!.GetValue<bool>());
            }
            else if (type == "stream_chunk")
            {
                var tr = ev["data"]?["state"]?["rawResponse"]?["toolResult"]?.AsObject();
                if (tr is not null)
                {
                    toolResults.Add(tr);
                }
            }
            else if (type == "stream_token")
            {
                tokens.Append(ev["token"]!.GetValue<string>());
            }
            else if (type == "eventual_response")
            {
                Assert.Equal(200, ev["status"]!.GetValue<int>());
                Assert.Equal("Done — record 42 was deleted.", ev["data"]!["data"]!["response"]!["responseParts"]![0]!.GetValue<string>());
                break;
            }
        }

        Assert.True(sawAck, "the confirm_tool_action ack must arrive");
        Assert.Equal("Done — record 42 was deleted.", tokens.ToString());
        // The approved tool actually ran — its real result reached the model.
        Assert.Contains(toolResults, tr =>
            tr["name"]!.GetValue<string>() == GatedTool && tr["result"]!.GetValue<string>().Contains("deleted", StringComparison.Ordinal));
        Assert.DoesNotContain(toolResults, tr => tr["result"]!.GetValue<string>().Contains("Denied by human", StringComparison.Ordinal));

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task RejectedConfirmation_BlocksTheTool_ButTurnCompletes()
    {
        await using var app = BuildApp();
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket);

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["message"] = "delete it",
        });

        var ack = await NextEventAsync(socket);
        Assert.Equal(202, ack["status"]!.GetValue<int>());

        var confirm = await NextEventAsync(socket);
        Assert.Equal("write_confirmation_required", confirm["type"]!.GetValue<string>());

        // Reject → the engine feeds the model a "Denied by human" result; the tool never runs, but
        // the turn still completes (no hang).
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "confirm_tool_action",
            ["requestId"] = "r-confirm",
            ["sessionId"] = sessionId,
            ["approved"] = false,
        });

        var toolResults = new List<JsonObject>();
        var sawRejectAck = false;
        while (true)
        {
            var ev = await NextEventAsync(socket);
            var type = ev["type"]!.GetValue<string>();
            if (type == "immediate_response" && ev["status"]!.GetValue<int>() == 200)
            {
                sawRejectAck = true;
                Assert.False(ev["data"]!["approved"]!.GetValue<bool>());
            }
            else if (type == "stream_chunk")
            {
                var tr = ev["data"]?["state"]?["rawResponse"]?["toolResult"]?.AsObject();
                if (tr is not null)
                {
                    toolResults.Add(tr);
                }
            }
            else if (type == "eventual_response")
            {
                break;
            }
        }

        Assert.True(sawRejectAck);
        // The rejected tool was blocked — the model saw a denial, not the result.
        Assert.Contains(toolResults, tr => tr["result"]!.GetValue<string>().Contains("Denied by human", StringComparison.Ordinal));
        Assert.DoesNotContain(toolResults, tr => tr["result"]!.GetValue<string>().Contains("Record 42 deleted", StringComparison.Ordinal));

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task ConfirmWithoutPending_IsACleanError()
    {
        await using var app = BuildApp();
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket);

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "confirm_tool_action",
            ["requestId"] = "r-confirm",
            ["sessionId"] = sessionId,
            ["approved"] = true,
        });

        var err = await NextEventAsync(socket);
        Assert.Equal("error", err["type"]!.GetValue<string>());
        Assert.Equal("NO_PENDING_CONFIRMATION", err["error"]!["code"]!.GetValue<string>());

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task ConfirmWithNonBoolApproved_FailsClosed()
    {
        await using var app = BuildApp();
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket);

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "confirm_tool_action",
            ["requestId"] = "r-confirm",
            ["sessionId"] = sessionId,
            ["approved"] = "yes", // not a boolean — must never be read as an approval
        });

        var err = await NextEventAsync(socket);
        Assert.Equal("error", err["type"]!.GetValue<string>());
        Assert.Equal("VALIDATION_ERROR", err["error"]!["code"]!.GetValue<string>());

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }
}
