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
/// Rich Interactions — the <c>identity_intake</c> kind's park → <c>interaction_required</c> →
/// <c>submit_interaction</c> → resume path AND its host EFFECT, driven end-to-end over a REAL WebSocket
/// against the in-process host. Beyond the choices coverage (park/resume/validate) this proves the
/// kind-routed host effect: a valid submit stamps the captured name/email/phone onto the session so a
/// later <c>end_user</c>-gated tool refusal is OTP-offered against the captured contact — even though the
/// session was created with NO email. The submitted values validate against the shared
/// <c>identity_intake</c> conformance fixture. The C# parity of the Rust identity-intake + otp seam tests.
/// </summary>
public class IdentityIntakeSubmitTests
{
    private const string AgentId = "11111111-1111-1111-1111-111111111111";

    // The identity_intake_values conformance fixture — the exact shared values the Rust server validates.
    private const string IntakeValuesJson = """{ "name": "Alice Example", "email": "alice@example.com", "phone": "+15551234567" }""";

    /// <summary>A host OTP service that records the contact it was handed — the assertion surface for the
    /// host effect (the captured email + phone must reach it). The server never sees the code.</summary>
    private sealed class RecordingOtpService : IOtpService
    {
        public OtpContact? LastContact { get; private set; }
        public int SendCount { get; private set; }

        public Task<OtpDelivery> SendOtpAsync(string sessionId, OtpContact contact, CancellationToken cancellationToken = default)
        {
            SendCount++;
            LastContact = contact;
            return Task.FromResult(new OtpDelivery(OtpChannel.Email, "a***@example.com"));
        }

        public Task<OtpVerifyOutcome> VerifyOtpAsync(string sessionId, string code, CancellationToken cancellationToken = default) =>
            Task.FromResult<OtpVerifyOutcome>(new OtpVerifyOutcome.Verified());
    }

    private static AITool AuthTool(string name, string result) =>
        AIFunctionFactory.Create(() => result, new AIFunctionFactoryOptions
        {
            Name = name,
            Description = $"{name} (declares auth support)",
            AdditionalProperties = new Dictionary<string, object?> { ["supportsAuthRequirement"] = true },
        });

    /// <summary>Build a host: turn 1 raises request_identity_intake (parks), turn 2 calls a public
    /// end_user tool (refused → OTP offered). A public agent with the gated tool + an OTP service.</summary>
    private static WebApplication BuildApp(RecordingOtpService otp)
    {
        var chat = new MockChatClient();
        // Turn 1: raise the intake (fields the fixture drives), then the resumed turn's closing text.
        chat.PushToolCall("call-intake", "request_identity_intake", new Dictionary<string, object?>
        {
            ["fields"] = new object?[] { "name", "email", "phone" },
            ["reason"] = "to send you the quote",
        });
        chat.PushText("Thanks, Alice — all set.");
        // Turn 2: call the end_user tool (unverified → refused → OTP offer), then closing text.
        chat.PushToolCall("call-user", "user_tool", new Dictionary<string, object?>());
        chat.PushText("All done.");

        var config = new AgentConfig(
            EnabledTools: new[] { new EnabledTool("user_tool", true, "end_user", null) },
            Visibility: "public");

        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton<IChatClient>(chat);
        builder.Services.AddSingleton<IReadOnlyList<AITool>>(new[] { AuthTool("user_tool", "REAL_USER_RESULT") });
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

    /// <summary>Create a session with the given render capabilities and NO email (the intake is the only
    /// way this session gets a contact).</summary>
    private static async Task<string> CreateSessionAsync(WebSocket socket, string[] supports)
    {
        var frame = new JsonObject
        {
            ["action"] = "create_conversation_session",
            ["requestId"] = "r-create",
            ["agentId"] = AgentId,
            ["supports"] = new JsonArray(supports.Select(s => (JsonNode)s).ToArray()),
        };
        await SendAsync(socket, frame);
        while (true)
        {
            var ev = await NextEventAsync(socket);
            if (ev["type"]!.GetValue<string>() == "immediate_response")
            {
                return ev["data"]!["sessionId"]!.GetValue<string>();
            }
        }
    }

    private static async Task<JsonObject> ReadUntilAsync(WebSocket socket, string type, List<JsonObject>? toolResults = null, List<string>? types = null)
    {
        while (true)
        {
            var ev = await NextEventAsync(socket);
            var t = ev["type"]!.GetValue<string>();
            types?.Add(t);
            if (toolResults is not null && t == "stream_chunk"
                && ev["data"]?["state"]?["rawResponse"]?["toolResult"]?.AsObject() is { } tr)
            {
                toolResults.Add(tr);
            }
            if (t == type)
            {
                return ev;
            }
        }
    }

    [Fact]
    public async Task RichSubmit_StampsContact_MakingSessionOtpContactable()
    {
        var otp = new RecordingOtpService();
        await using var app = BuildApp(otp);
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket, supports: new[] { "identity_form" });

        // Turn 1: the raise parks the turn with an identity_intake card.
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg-1",
            ["sessionId"] = sessionId,
            ["message"] = "I'd like a quote",
        });

        var required = await ReadUntilAsync(socket, "interaction_required");
        var payload = required["data"]!["data"]!;
        var interactionId = payload["interactionId"]!.GetValue<string>();
        Assert.Equal("identity_intake", payload["kind"]!.GetValue<string>());
        Assert.Equal("to send you the quote", payload["reason"]!.GetValue<string>());
        Assert.Equal("email", payload["spec"]!["fields"]![1]!["key"]!.GetValue<string>());

        // Submit the shared identity_intake_values fixture → validates + resumes + fires the host effect.
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "submit_interaction",
            ["requestId"] = "r-msg-1",
            ["sessionId"] = sessionId,
            ["interactionId"] = interactionId,
            ["kind"] = "identity_intake",
            ["values"] = JsonNode.Parse(IntakeValuesJson),
        });

        var toolResults = new List<JsonObject>();
        var ack = await ReadUntilAsync(socket, "immediate_response", toolResults);
        Assert.Equal(200, ack["status"]!.GetValue<int>());
        Assert.Equal("identity_intake", ack["data"]!["kind"]!.GetValue<string>());

        var final1 = await ReadUntilAsync(socket, "eventual_response", toolResults);
        Assert.Equal(200, final1["status"]!.GetValue<int>());
        // The raise tool resumed with the canonical, normalized payload. (System.Text.Json escapes the
        // '+' as + in the tool-result string, so match the digits, not the leading '+'.)
        Assert.Contains(toolResults, tr =>
            tr["name"]!.GetValue<string>() == "request_identity_intake"
            && tr["result"]!.GetValue<string>().Contains("submitted", StringComparison.Ordinal)
            && tr["result"]!.GetValue<string>().Contains("15551234567", StringComparison.Ordinal));

        // Turn 2: the end_user tool is refused (unverified) → the OTP offer fires — proving the host
        // effect: the session had NO email at create; only the intake gave it one (+ a phone → SMS).
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg-2",
            ["sessionId"] = sessionId,
            ["message"] = "run it",
        });

        var types = new List<string>();
        await ReadUntilAsync(socket, "eventual_response", types: types);
        Assert.Contains("otp_verification_required", types);
        Assert.Contains("otp_sent", types);
        Assert.Equal(1, otp.SendCount);

        // The captured contact (both channels) reached the OTP service — the host effect's payload.
        Assert.Equal("alice@example.com", otp.LastContact!.Email);
        Assert.Equal("+15551234567", otp.LastContact.Phone);
        Assert.Equal(new[] { OtpChannel.Email, OtpChannel.Sms }, otp.LastContact.AvailableChannels);

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task TextOnlyChannel_ConversationalSubmit_AlsoStampsContact()
    {
        var otp = new RecordingOtpService();
        // Rebuild the mock for the fallback flow: raise (returns directive, no park), then the model
        // submits via the submit_interaction TOOL, then closing text; turn 2 calls the gated tool.
        var chat = new MockChatClient();
        chat.PushToolCall("call-intake", "request_identity_intake", new Dictionary<string, object?>
        {
            ["fields"] = new object?[] { "email", "phone" },
            ["reason"] = "to text you the quote",
        });
        chat.PushToolCall("call-submit", "submit_interaction", new Dictionary<string, object?>
        {
            ["kind"] = "identity_intake",
            ["values"] = new Dictionary<string, object?> { ["email"] = "alice@example.com", ["phone"] = "555-123-4567" },
        });
        chat.PushText("Got it.");
        chat.PushToolCall("call-user", "user_tool", new Dictionary<string, object?>());
        chat.PushText("All done.");

        var config = new AgentConfig(
            EnabledTools: new[] { new EnabledTool("user_tool", true, "end_user", null) },
            Visibility: "public");

        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton<IChatClient>(chat);
        builder.Services.AddSingleton<IReadOnlyList<AITool>>(new[] { AuthTool("user_tool", "REAL_USER_RESULT") });
        builder.Services.AddSingleton<IAgentConfigResolver>(new StaticAgentConfigResolver().Set(AgentId, config));
        builder.Services.AddSingleton<IOtpService>(otp);
        builder.Services.AddSmoothOperatorServer();
        await using var app = builder.Build();
        app.MapSmoothOperatorWebSocket("/ws");
        await app.StartAsync();

        using var socket = await ConnectAsync(app.GetTestServer());
        // No `supports` for identity_form → the raise degrades to the conversational fallback.
        var sessionId = await CreateSessionAsync(socket, supports: Array.Empty<string>());

        // Turn 1: raise degrades to a directive; the model then submits via the tool (no card, no park).
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg-1",
            ["sessionId"] = sessionId,
            ["message"] = "text me a quote",
        });
        var types1 = new List<string>();
        await ReadUntilAsync(socket, "eventual_response", types: types1);
        Assert.DoesNotContain("interaction_required", types1);

        // Turn 2: the gated tool is refused → OTP offered against the contact the fallback submit stamped.
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg-2",
            ["sessionId"] = sessionId,
            ["message"] = "run it",
        });
        var types2 = new List<string>();
        await ReadUntilAsync(socket, "eventual_response", types: types2);
        Assert.Contains("otp_sent", types2);
        Assert.Equal("alice@example.com", otp.LastContact!.Email);
        Assert.Equal("+15551234567", otp.LastContact.Phone); // 555-123-4567 normalized to E.164

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }
}
