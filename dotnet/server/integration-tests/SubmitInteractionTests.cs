using System.Net.WebSockets;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.TestHost;
using Microsoft.Extensions.AI;
using Microsoft.Extensions.DependencyInjection;
using SmooAI.SmoothOperator.Server.AspNetCore;

namespace SmooAI.SmoothOperator.Server.IntegrationTests;

/// <summary>
/// Rich Interactions — the <c>choices</c> kind's park → <c>interaction_required</c> →
/// <c>submit_interaction</c> → resume path, driven end-to-end over a REAL WebSocket against the
/// in-process ASP.NET Core host. The C# parity of the Rust <c>tests</c> for the interaction runtime.
///
/// The turn runs offline (a scripted <see cref="MockChatClient"/> calls <c>request_choices</c>), so
/// there is no gateway. The <c>submit_interaction</c> frame arrives on the same connection's reader
/// while the turn is parked — proving the turn runs as a background task. Covers: the rich (card) path
/// with a valid submit, a retryable invalid submit (<c>interaction_invalid</c>, turn stays parked) then
/// a valid resubmit, a decline, and the text-only FALLBACK (no capability → no card, the raise returns
/// the conversational directive instead). The submitted values validate against the shared
/// <c>choices</c> conformance fixture.
/// </summary>
public class SubmitInteractionTests
{
    // The choices_spec conformance fixture's questions — the exact shared spec the Rust server uses.
    private const string ChoicesArgsJson = """
        {
            "questions": [
                { "question": "Which plan are you interested in?", "header": "Plan",
                  "options": [ { "label": "Basic", "description": "For individuals" }, { "label": "Pro", "description": "For growing teams" } ] },
                { "question": "What topics can we help with?", "header": "Topics",
                  "options": [ { "label": "Sales" }, { "label": "Support" }, { "label": "Billing" } ], "multiSelect": true }
            ],
            "reason": "to route you to the right team"
        }
        """;

    private static WebApplication BuildApp()
    {
        var chat = new MockChatClient();
        var args = JsonSerializer.Deserialize<JsonElement>(ChoicesArgsJson);
        chat.PushToolCall("call-1", "request_choices", new Dictionary<string, object?>
        {
            ["questions"] = args.GetProperty("questions"),
            ["reason"] = args.GetProperty("reason"),
        });
        chat.PushText("Great — routing you to the right team now.");

        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton<IChatClient>(chat);
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

    private static async Task<string> CreateSessionAsync(WebSocket socket, string[]? supports = null)
    {
        var frame = new JsonObject
        {
            ["action"] = "create_conversation_session",
            ["requestId"] = "r-create",
            ["agentId"] = "11111111-1111-1111-1111-111111111111",
            ["userName"] = "Alice",
            ["userEmail"] = "alice@example.com",
        };
        if (supports is not null)
        {
            frame["supports"] = new JsonArray(supports.Select(s => (JsonNode)s).ToArray());
        }
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

    /// <summary>Read events until one of <paramref name="types"/> arrives — so a test can assert on
    /// WHICH of two possible outcomes settled instead of blocking on the one it hopes for.</summary>
    private static async Task<JsonObject> ReadUntilAnyAsync(WebSocket socket, params string[] types)
    {
        while (true)
        {
            var ev = await NextEventAsync(socket);
            if (types.Contains(ev["type"]!.GetValue<string>()))
            {
                return ev;
            }
        }
    }

    /// <summary>Read events until the given type, returning that event; collects any tool results seen.</summary>
    private static async Task<JsonObject> ReadUntilAsync(WebSocket socket, string type, List<JsonObject>? toolResults = null)
    {
        while (true)
        {
            var ev = await NextEventAsync(socket);
            var t = ev["type"]!.GetValue<string>();
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
    public async Task RichPath_ParksOnCard_Resumes_OnValidSubmit()
    {
        await using var app = BuildApp();
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket, supports: new[] { "choice_chips" });

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["message"] = "I need help choosing",
        });

        // The raise tool parks the turn: interaction_required carries the kind, spec, and reason.
        var required = await ReadUntilAsync(socket, "interaction_required");
        Assert.Equal("r-msg", required["requestId"]!.GetValue<string>());
        var payload = required["data"]!["data"]!;
        var interactionId = payload["interactionId"]!.GetValue<string>();
        Assert.False(string.IsNullOrEmpty(interactionId));
        Assert.Equal("choices", payload["kind"]!.GetValue<string>());
        Assert.Equal("to route you to the right team", payload["reason"]!.GetValue<string>());
        Assert.Equal("Plan", payload["spec"]!["questions"]![0]!["header"]!.GetValue<string>());

        // Submit the shared choices_values fixture (validates against choices_spec → choices_payload).
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "submit_interaction",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["interactionId"] = interactionId,
            ["kind"] = "choices",
            ["values"] = JsonNode.Parse("""
                { "answers": [ { "header": "Plan", "options": ["Pro"] },
                               { "header": "Topics", "options": ["Sales", "Billing"], "other": "Partnerships" } ] }
                """),
        });

        // The submit is acked, and the parked turn resumes to completion.
        var toolResults = new List<JsonObject>();
        var ack = await ReadUntilAsync(socket, "immediate_response", toolResults);
        Assert.Equal(200, ack["status"]!.GetValue<int>());
        Assert.Equal("choices", ack["data"]!["kind"]!.GetValue<string>());

        var final = await ReadUntilAsync(socket, "eventual_response", toolResults);
        Assert.Equal("Great — routing you to the right team now.",
            final["data"]!["data"]!["response"]!["responseParts"]![0]!.GetValue<string>());

        // The raise tool's result — the validated, canonicalized payload — reached the model.
        Assert.Contains(toolResults, tr =>
            tr["name"]!.GetValue<string>() == "request_choices"
            && tr["result"]!.GetValue<string>().Contains("submitted", StringComparison.Ordinal)
            && tr["result"]!.GetValue<string>().Contains("Partnerships", StringComparison.Ordinal));

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task RichPath_InvalidSubmit_StaysParked_ThenResumesOnValid()
    {
        await using var app = BuildApp();
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket, supports: new[] { "choice_chips" });

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["message"] = "help me choose",
        });

        var required = await ReadUntilAsync(socket, "interaction_required");
        var interactionId = required["data"]!["data"]!["interactionId"]!.GetValue<string>();

        // Invalid: an option that isn't offered → retryable interaction_invalid, turn STAYS parked.
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "submit_interaction",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["interactionId"] = interactionId,
            ["values"] = JsonNode.Parse("""{ "answers": [ { "header": "Plan", "options": ["Platinum"] }, { "header": "Topics", "options": ["Sales"] } ] }"""),
        });

        var invalid = await ReadUntilAsync(socket, "interaction_invalid");
        Assert.Equal("r-msg", invalid["requestId"]!.GetValue<string>());
        var invData = invalid["data"]!["data"]!;
        Assert.Equal(interactionId, invData["interactionId"]!.GetValue<string>());
        Assert.Equal("choices", invData["kind"]!.GetValue<string>());
        Assert.NotEmpty(invData["errors"]!.AsArray());
        Assert.Equal("Plan", invData["errors"]![0]!["field"]!.GetValue<string>());

        // Resubmit the same interactionId with valid values → the turn resumes.
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "submit_interaction",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["interactionId"] = interactionId,
            ["values"] = JsonNode.Parse("""{ "answers": [ { "header": "Plan", "options": ["Pro"] }, { "header": "Topics", "options": ["Sales"] } ] }"""),
        });

        var final = await ReadUntilAsync(socket, "eventual_response");
        Assert.Equal(200, final["status"]!.GetValue<int>());

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    /// <summary>
    /// Malformed <c>values</c> must be a RETRYABLE <c>interaction_invalid</c>, never a terminal
    /// <c>INTERNAL_ERROR</c>. An explicit JSON <c>null</c> SETS a <c>[JsonPropertyName]</c>-annotated
    /// <c>List&lt;T&gt;</c>/<c>string</c> property (it does not leave its <c>= new()</c> initializer
    /// alone), so these three shapes dereferenced null inside <c>ChoicesKind.Validate</c> — OUTSIDE its
    /// <c>catch (JsonException)</c>. That landed in the dispatcher's generic handler as an
    /// <c>INTERNAL_ERROR</c> with no <c>Resolve</c>: the turn stayed parked for the FULL park timeout
    /// with the client never told what to fix. Rust returns a retryable <c>interaction_invalid</c>
    /// (<c>choices.rs</c>). th-acf8ea.
    /// </summary>
    [Theory]
    [InlineData("""{ "answers": null }""")]
    [InlineData("""{ "answers": [ { "header": null, "options": ["Pro"] } ] }""")]
    [InlineData("""{ "answers": [ { "header": "Plan", "options": null } ] }""")]
    [InlineData("""{ "answers": [ { "header": "Plan", "options": [null] } ] }""")]
    public async Task RichPath_NullValuesShape_IsRetryableInvalid_NotInternalError(string valuesJson)
    {
        await using var app = BuildApp();
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket, supports: new[] { "choice_chips" });

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["message"] = "help me choose",
        });

        var required = await ReadUntilAsync(socket, "interaction_required");
        var interactionId = required["data"]!["data"]!["interactionId"]!.GetValue<string>();

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "submit_interaction",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["interactionId"] = interactionId,
            ["values"] = JsonNode.Parse(valuesJson),
        });

        // Read until the submit settles EITHER way, so the buggy INTERNAL_ERROR fails the assert
        // instead of hanging until the park's own 300s backstop.
        var settled = await ReadUntilAnyAsync(socket, "interaction_invalid", "error").WaitAsync(TimeSpan.FromSeconds(30));
        Assert.Equal("interaction_invalid", settled["type"]!.GetValue<string>());
        Assert.NotEmpty(settled["data"]!["data"]!["errors"]!.AsArray());

        // Retryable means the park SURVIVED: the same interactionId still resolves the turn.
        await SendAsync(socket, new JsonObject
        {
            ["action"] = "submit_interaction",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["interactionId"] = interactionId,
            ["values"] = JsonNode.Parse("""{ "answers": [ { "header": "Plan", "options": ["Pro"] }, { "header": "Topics", "options": ["Sales"] } ] }"""),
        });

        var final = await ReadUntilAsync(socket, "eventual_response").WaitAsync(TimeSpan.FromSeconds(30));
        Assert.Equal(200, final["status"]!.GetValue<int>());

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task RichPath_Decline_ResumesTheTurn()
    {
        await using var app = BuildApp();
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket, supports: new[] { "choice_chips" });

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["message"] = "help me choose",
        });

        var required = await ReadUntilAsync(socket, "interaction_required");
        var interactionId = required["data"]!["data"]!["interactionId"]!.GetValue<string>();

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "submit_interaction",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["interactionId"] = interactionId,
            ["declined"] = true,
        });

        var toolResults = new List<JsonObject>();
        var ack = await ReadUntilAsync(socket, "immediate_response", toolResults);
        Assert.Equal(200, ack["status"]!.GetValue<int>());
        var final = await ReadUntilAsync(socket, "eventual_response", toolResults);
        Assert.Equal(200, final["status"]!.GetValue<int>());
        // The raise tool resumed with a declined payload.
        Assert.Contains(toolResults, tr =>
            tr["name"]!.GetValue<string>() == "request_choices"
            && tr["result"]!.GetValue<string>().Contains("declined", StringComparison.Ordinal));

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task TextOnlyChannel_DegradesToConversationalFallback()
    {
        await using var app = BuildApp();
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        // No `supports` → the choice_chips capability is absent → the raise degrades to fallback.
        var sessionId = await CreateSessionAsync(socket, supports: null);

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "send_message",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["message"] = "help me choose",
        });

        // The turn completes WITHOUT ever parking: the raise tool returns the conversational directive
        // and there is no interaction_required, no submit needed.
        var toolResults = new List<JsonObject>();
        var sawInteractionRequired = false;
        while (true)
        {
            var ev = await NextEventAsync(socket);
            var type = ev["type"]!.GetValue<string>();
            if (type == "interaction_required")
            {
                sawInteractionRequired = true;
            }
            else if (type == "stream_chunk"
                && ev["data"]?["state"]?["rawResponse"]?["toolResult"]?.AsObject() is { } tr)
            {
                toolResults.Add(tr);
            }
            else if (type == "eventual_response")
            {
                break;
            }
        }

        Assert.False(sawInteractionRequired, "a text-only session must never receive an interaction card");
        Assert.Contains(toolResults, tr =>
            tr["name"]!.GetValue<string>() == "request_choices"
            && tr["result"]!.GetValue<string>().Contains("cannot display choice chips", StringComparison.Ordinal));

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    [Fact]
    public async Task SubmitWithoutPending_IsACleanError()
    {
        await using var app = BuildApp();
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());
        var sessionId = await CreateSessionAsync(socket, supports: new[] { "choice_chips" });

        await SendAsync(socket, new JsonObject
        {
            ["action"] = "submit_interaction",
            ["requestId"] = "r-msg",
            ["sessionId"] = sessionId,
            ["interactionId"] = "00000000-0000-0000-0000-000000000000",
            ["values"] = JsonNode.Parse("""{ "answers": [] }"""),
        });

        var err = await NextEventAsync(socket);
        Assert.Equal("error", err["type"]!.GetValue<string>());
        Assert.Equal("NO_PENDING_INTERACTION", err["error"]!["code"]!.GetValue<string>());

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }
}
