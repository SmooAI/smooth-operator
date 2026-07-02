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
/// The OTP identity flow enforced end-to-end over a REAL WebSocket against the in-process host: a
/// public agent's <c>end_user</c> tool refused for lack of a verified session triggers a post-turn
/// OTP offer (<c>otp_verification_required</c> → <c>otp_sent</c>), a <c>verify_otp</c> marks the
/// session authenticated, and the re-sent message then runs the gated tool. Admin refusals are never
/// offered OTP; a session with no contact is never offered OTP. The turn runs offline (scripted
/// MockChatClient). The C# analog of the Rust <c>otp_flow.rs</c> integration coverage.
/// </summary>
public class OtpFlowIntegrationTests
{
    private const string AgentId = "11111111-1111-1111-1111-111111111111";

    private sealed record Session(string SessionId, string ConversationId);

    /// <summary>A host OTP service: delivers to email, verifies a fixed code. The server never sees
    /// the code — this stands in for the host's real code store / channel.</summary>
    private sealed class FakeOtpService : IOtpService
    {
        public int SendCount { get; private set; }

        public Task<OtpDelivery> SendOtpAsync(string sessionId, OtpContact contact, CancellationToken cancellationToken = default)
        {
            SendCount++;
            return Task.FromResult(new OtpDelivery(OtpChannel.Email, "j***@example.com"));
        }

        public Task<OtpVerifyOutcome> VerifyOtpAsync(string sessionId, string code, CancellationToken cancellationToken = default) =>
            Task.FromResult<OtpVerifyOutcome>(code == "123456"
                ? new OtpVerifyOutcome.Verified()
                : new OtpVerifyOutcome.Invalid(2, OtpError.InvalidCode, "Invalid code. 2 attempt(s) remaining."));
    }

    private static AITool AuthTool(string name, string result) =>
        AIFunctionFactory.Create(() => result, new AIFunctionFactoryOptions
        {
            Name = name,
            Description = $"{name} (declares auth support)",
            AdditionalProperties = new Dictionary<string, object?> { ["supportsAuthRequirement"] = true },
        });

    private static AgentConfig WithTool(string toolId, string authLevel, string visibility) =>
        new(EnabledTools: new[] { new EnabledTool(toolId, true, authLevel, null) }, Visibility: visibility);

    /// <summary>Build a host whose scripted model calls <paramref name="toolToCall"/> on every turn
    /// (up to <paramref name="turns"/> turns), with an OTP service registered.</summary>
    private static WebApplication BuildApp(AgentConfig config, AITool tool, string toolToCall, FakeOtpService otp, int turns = 1)
    {
        var chat = new MockChatClient();
        for (var i = 0; i < turns; i++)
        {
            chat.PushToolCall($"call-{i}", toolToCall, new Dictionary<string, object?>());
            chat.PushText("All done.");
        }

        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton<IChatClient>(chat);
        builder.Services.AddSingleton<IReadOnlyList<AITool>>(new[] { tool });
        builder.Services.AddSingleton<IAgentConfigResolver>(new StaticAgentConfigResolver().Set(AgentId, config));
        builder.Services.AddSingleton<IOtpService>(otp);
        builder.Services.AddSmoothOperatorServer();

        var app = builder.Build();
        app.MapSmoothOperatorWebSocket("/ws");
        return app;
    }

    private static async Task<WebSocket> ConnectAsync(TestServer server) =>
        await server.CreateWebSocketClient().ConnectAsync(new Uri(server.BaseAddress, "ws"), CancellationToken.None);

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

    private static async Task<Session> CreateSessionAsync(WebSocket socket, string? userEmail)
    {
        var frame = new JsonObject { ["action"] = "create_conversation_session", ["requestId"] = "r-create", ["agentId"] = AgentId, ["userName"] = "Alice" };
        if (userEmail is not null)
        {
            frame["userEmail"] = userEmail;
        }
        await SendAsync(socket, frame);
        while (true)
        {
            var ev = await NextEventAsync(socket);
            if (ev["type"]!.GetValue<string>() == "immediate_response")
            {
                return new Session(ev["data"]!["sessionId"]!.GetValue<string>(), ev["data"]!["conversationId"]!.GetValue<string>());
            }
        }
    }

    /// <summary>Send one message; collect every event type in order up to and including the terminal
    /// eventual_response, plus the tool-result strings.</summary>
    private static async Task<(List<string> Types, List<string> ToolResults)> SendMessageAsync(WebSocket socket, string sessionId, string requestId = "r-msg")
    {
        await SendAsync(socket, new JsonObject { ["action"] = "send_message", ["requestId"] = requestId, ["sessionId"] = sessionId, ["message"] = "go" });

        var types = new List<string>();
        var toolResults = new List<string>();
        while (true)
        {
            var ev = await NextEventAsync(socket);
            var type = ev["type"]!.GetValue<string>();
            types.Add(type);
            if (type == "stream_chunk")
            {
                var tr = ev["data"]?["state"]?["rawResponse"]?["toolResult"]?.AsObject();
                if (tr is not null)
                {
                    toolResults.Add(tr["result"]!.GetValue<string>());
                }
            }
            if (type == "eventual_response")
            {
                return (types, toolResults);
            }
        }
    }

    private static Task VerifyOtpAsync(WebSocket socket, string sessionId, string code) =>
        SendAsync(socket, new JsonObject { ["action"] = "verify_otp", ["requestId"] = "r-verify", ["sessionId"] = sessionId, ["code"] = code });

    [Fact]
    public async Task EndUserRefusal_WithContact_OffersOtp_InOrder_BeforeTerminal()
    {
        var otp = new FakeOtpService();
        await using var app = BuildApp(WithTool("user_tool", "end_user", "public"), AuthTool("user_tool", "REAL_USER_RESULT"), "user_tool", otp);
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var session = await CreateSessionAsync(socket, "alice@example.com");

        var (types, toolResults) = await SendMessageAsync(socket, session.SessionId);

        // The gated tool never ran (blocked, unverified).
        Assert.DoesNotContain(toolResults, r => r.Contains("REAL_USER_RESULT", StringComparison.Ordinal));
        // Offer sequence: otp_verification_required → otp_sent → eventual_response, in that order.
        var vr = types.IndexOf("otp_verification_required");
        var sent = types.IndexOf("otp_sent");
        var terminal = types.IndexOf("eventual_response");
        Assert.True(vr >= 0 && sent > vr && terminal > sent, $"expected required<sent<terminal, got: {string.Join(",", types)}");
        Assert.Equal(1, otp.SendCount);

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task AdminRefusal_IsNeverOfferedOtp()
    {
        var otp = new FakeOtpService();
        await using var app = BuildApp(WithTool("admin_tool", "admin", "public"), AuthTool("admin_tool", "REAL_ADMIN_RESULT"), "admin_tool", otp);
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var session = await CreateSessionAsync(socket, "alice@example.com");

        var (types, toolResults) = await SendMessageAsync(socket, session.SessionId);

        Assert.DoesNotContain(toolResults, r => r.Contains("REAL_ADMIN_RESULT", StringComparison.Ordinal));
        Assert.DoesNotContain("otp_verification_required", types);
        Assert.DoesNotContain("otp_sent", types);
        Assert.Equal(0, otp.SendCount);

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task EndUserRefusal_WithNoContact_IsNotOfferedOtp()
    {
        var otp = new FakeOtpService();
        await using var app = BuildApp(WithTool("user_tool", "end_user", "public"), AuthTool("user_tool", "REAL_USER_RESULT"), "user_tool", otp);
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var session = await CreateSessionAsync(socket, userEmail: null);

        var (types, _) = await SendMessageAsync(socket, session.SessionId);

        // No channel to deliver a code → the server can't offer OTP (still fail-closed refused).
        Assert.DoesNotContain("otp_verification_required", types);
        Assert.Equal(0, otp.SendCount);

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task VerifyOtp_ThenResend_RunsTheGatedTool()
    {
        var otp = new FakeOtpService();
        await using var app = BuildApp(WithTool("user_tool", "end_user", "public"), AuthTool("user_tool", "REAL_USER_RESULT"), "user_tool", otp, turns: 2);
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var session = await CreateSessionAsync(socket, "alice@example.com");

        // First turn: refused → offered OTP, tool did NOT run.
        var (firstTypes, firstResults) = await SendMessageAsync(socket, session.SessionId, "r-msg-1");
        Assert.Contains("otp_verification_required", firstTypes);
        Assert.DoesNotContain(firstResults, r => r.Contains("REAL_USER_RESULT", StringComparison.Ordinal));

        // Verify the code → otp_verified marks the session authenticated.
        await VerifyOtpAsync(socket, session.SessionId, "123456");
        var verified = await NextEventAsync(socket);
        Assert.Equal("otp_verified", verified["type"]!.GetValue<string>());

        // Re-send: the now-verified session's end_user tool runs.
        var (_, secondResults) = await SendMessageAsync(socket, session.SessionId, "r-msg-2");
        Assert.Contains(secondResults, r => r.Contains("REAL_USER_RESULT", StringComparison.Ordinal));

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }
}
