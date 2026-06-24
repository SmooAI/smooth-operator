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
/// Scenario parity runner — the C# port of <c>python/server/tests/test_scenario_parity.py</c>.
///
/// Runs every scenario in <c>spec/conformance/scenarios/*.json</c> through the C# server and asserts
/// the normalized protocol output matches. This is the shared corpus that holds the five native servers
/// (Rust · C# · Python · TypeScript · Go) to parity: when all five run this corpus green, the servers
/// are at protocol parity. The turn is deterministic because the engine runs on the same scripted mock
/// chat client the scenario declares (<c>mockLlmScript</c>) — no gateway, no flakiness.
/// </summary>
public class ScenarioParityTests
{
    private static readonly string ScenariosDir = ResolveScenariosDir();

    public static IEnumerable<object[]> Scenarios()
    {
        foreach (var path in Directory.EnumerateFiles(ScenariosDir, "*.json").OrderBy(p => p, StringComparer.Ordinal))
        {
            yield return new object[] { Path.GetFileName(path), path };
        }
    }

    [Theory]
    [MemberData(nameof(Scenarios))]
    public async Task ScenarioParity(string name, string path)
    {
        _ = name; // surfaced as the test id via MemberData
        var scenario = JsonNode.Parse(await File.ReadAllTextAsync(path))!.AsObject();
        var chat = BuildMock(scenario["mockLlmScript"]?.AsArray());

        await using var app = BuildApp(chat);
        await app.StartAsync();
        using var socket = await ConnectAsync(app.GetTestServer());

        var vars = new Dictionary<string, JsonNode?>();
        foreach (var step in scenario["steps"]!.AsArray())
        {
            var stepObj = step!.AsObject();
            var send = Subst(stepObj["send"]!, vars);
            await SendAsync(socket, send.ToJsonString());
            await MatchExpectedAsync(socket, stepObj["expect"]!.AsArray(), vars);
        }

        await socket.CloseAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        await app.StopAsync();
    }

    /// <summary>Seed the scripted mock chat client from the scenario's <c>mockLlmScript</c>.</summary>
    private static MockChatClient BuildMock(JsonArray? script)
    {
        var mock = new MockChatClient();
        if (script is null)
        {
            return mock;
        }

        foreach (var entry in script)
        {
            var obj = entry!.AsObject();
            var kind = obj["kind"]!.GetValue<string>();
            switch (kind)
            {
                case "text":
                    mock.PushText(obj["text"]!.GetValue<string>());
                    break;
                case "toolCall":
                    mock.PushToolCall(
                        obj["id"]?.GetValue<string>() ?? "call-1",
                        obj["name"]!.GetValue<string>(),
                        ParseToolArguments(obj["arguments"]));
                    break;
                default:
                    throw new InvalidOperationException($"unknown mockLlmScript kind: '{kind}'");
            }
        }

        return mock;
    }

    /// <summary>
    /// A scenario tool call's <c>arguments</c> is a JSON-object string (per the spec) — parse it into the
    /// argument dictionary the mock expects. Tolerate an inline object too.
    /// </summary>
    private static IDictionary<string, object?> ParseToolArguments(JsonNode? arguments)
    {
        if (arguments is null)
        {
            return new Dictionary<string, object?>();
        }

        var obj = arguments is JsonValue value && value.TryGetValue<string>(out var json)
            ? JsonNode.Parse(json)!.AsObject()
            : arguments.AsObject();

        var result = new Dictionary<string, object?>();
        foreach (var (key, node) in obj)
        {
            result[key] = node is null ? null : JsonSerializer.Deserialize<object?>(node.ToJsonString());
        }

        return result;
    }

    /// <summary>Match the outbound event stream against an ordered list of matchers (ports the Python runner).</summary>
    private static async Task MatchExpectedAsync(WebSocket socket, JsonArray matchers, Dictionary<string, JsonNode?> vars)
    {
        JsonObject? pending = null; // one-event lookahead when a `repeat` matcher overruns
        foreach (var matcherNode in matchers)
        {
            var matcher = matcherNode!.AsObject();
            var type = matcher["type"]!.GetValue<string>();
            var repeat = matcher["repeat"]?.GetValue<bool>() ?? false;
            var accumulateField = matcher["accumulate"]?.GetValue<string>();
            var accumulated = new StringBuilder();

            while (true)
            {
                var ev = pending ?? await NextEventAsync(socket);
                pending = null;

                if (repeat && ev["type"]!.GetValue<string>() != type)
                {
                    // the repeated run ended; this event belongs to the next matcher
                    pending = ev;
                    break;
                }

                Assert.Equal(type, ev["type"]!.GetValue<string>());

                if (matcher["status"] is JsonNode status)
                {
                    Assert.Equal(status.GetValue<int>(), ev["status"]!.GetValue<int>());
                }

                if (matcher["statusGte"] is JsonNode statusGte)
                {
                    Assert.True(ev["status"]!.GetValue<int>() >= statusGte.GetValue<int>(),
                        $"{type}: status {ev["status"]!.GetValue<int>()} < {statusGte.GetValue<int>()}");
                }

                if (matcher["assert"] is JsonObject asserts)
                {
                    foreach (var (dotPath, expected) in asserts)
                    {
                        var actual = Dot(ev, dotPath);
                        Assert.True(JsonEquals(actual, expected),
                            $"{type}: {dotPath} = {actual?.ToJsonString() ?? "null"} != {expected?.ToJsonString() ?? "null"}");
                    }
                }

                if (matcher["capture"] is JsonObject captures)
                {
                    foreach (var (var, dotPathNode) in captures)
                    {
                        vars[var] = Dot(ev, dotPathNode!.GetValue<string>());
                    }
                }

                if (accumulateField is not null)
                {
                    accumulated.Append(Dot(ev, accumulateField)!.GetValue<string>());
                }

                if (!repeat)
                {
                    break;
                }
            }

            if (matcher["assertAccumulated"] is JsonNode assertAccumulated)
            {
                Assert.Equal(assertAccumulated.GetValue<string>(), accumulated.ToString());
            }
        }
    }

    /// <summary>Next protocol event, skipping non-semantic keepalive/pong frames (matches the Python runner).</summary>
    private static async Task<JsonObject> NextEventAsync(WebSocket socket)
    {
        while (true)
        {
            var ev = await ReceiveAsync(socket);
            var type = ev["type"]?.GetValue<string>();
            if (type is not ("keepalive" or "pong"))
            {
                return ev;
            }
        }
    }

    /// <summary>Resolve a dotted path (<c>data.data.response.responseParts</c>) into a nested node.</summary>
    private static JsonNode? Dot(JsonObject root, string path)
    {
        JsonNode? cur = root;
        foreach (var part in path.Split('.'))
        {
            cur = cur!.AsObject()[part];
        }

        return cur;
    }

    /// <summary>Replace <c>{{name}}</c> placeholders in string fields from captured vars (recursively).</summary>
    private static JsonNode Subst(JsonNode value, Dictionary<string, JsonNode?> vars)
    {
        switch (value)
        {
            case JsonObject obj:
            {
                var result = new JsonObject();
                foreach (var (key, child) in obj)
                {
                    result[key] = child is null ? null : Subst(child, vars);
                }

                return result;
            }
            case JsonValue v when v.TryGetValue<string>(out var s) && s.StartsWith("{{", StringComparison.Ordinal) && s.EndsWith("}}", StringComparison.Ordinal):
            {
                var resolved = vars[s[2..^2]];
                return resolved is null ? JsonValue.Create((string?)null)! : resolved.DeepClone();
            }
            default:
                return value.DeepClone();
        }
    }

    /// <summary>Structural JSON equality — compares scalars and arrays/objects by their canonical text.</summary>
    private static bool JsonEquals(JsonNode? a, JsonNode? b)
    {
        if (a is null || b is null)
        {
            return a is null && b is null;
        }

        return JsonNode.DeepEquals(a, b);
    }

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

    private static async Task<WebSocket> ConnectAsync(TestServer server)
    {
        var client = server.CreateWebSocketClient();
        return await client.ConnectAsync(new Uri(server.BaseAddress, "ws"), CancellationToken.None);
    }

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

    /// <summary>Walk up from the test assembly to the repo root and resolve the shared scenarios dir.</summary>
    private static string ResolveScenariosDir()
    {
        var dir = AppContext.BaseDirectory;
        while (dir is not null)
        {
            var candidate = Path.Combine(dir, "spec", "conformance", "scenarios");
            if (Directory.Exists(candidate))
            {
                return candidate;
            }

            dir = Path.GetDirectoryName(dir.TrimEnd(Path.DirectorySeparatorChar));
        }

        throw new DirectoryNotFoundException("could not locate spec/conformance/scenarios from " + AppContext.BaseDirectory);
    }
}
