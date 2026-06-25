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
        var serverDirective = scenario["server"]?.AsObject();
        var tools = BuildTools(serverDirective?["tools"]?.AsArray());
        var confirmTools = BuildConfirmTools(serverDirective?["confirmTools"]?.AsArray());
        var knowledge = BuildKnowledge(serverDirective?["knowledge"]?.AsArray());

        await using var app = BuildApp(chat, tools, confirmTools, knowledge);
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
    /// Build deterministic test tools from a scenario's <c>server.tools</c> directive. Each tool ignores
    /// its arguments and returns the spec's fixed <c>result</c> string, so a tool-call turn is fully
    /// deterministic across every server. The C# analog of the Python runner's <c>_build_tools</c>.
    /// </summary>
    private static IReadOnlyList<AITool> BuildTools(JsonArray? specs)
    {
        if (specs is null)
        {
            return Array.Empty<AITool>();
        }

        var tools = new List<AITool>();
        foreach (var specNode in specs)
        {
            var spec = specNode!.AsObject();
            var result = spec["result"]!.GetValue<string>();
            tools.Add(AIFunctionFactory.Create(
                () => result,
                spec["name"]!.GetValue<string>(),
                spec["description"]?.GetValue<string>() ?? string.Empty));
        }

        return tools;
    }

    /// <summary>
    /// Build the write-confirmation HITL tool-name patterns from a scenario's <c>server.confirmTools</c>
    /// directive. When non-empty the server gates each matching tool behind a <c>confirm_tool_action</c>
    /// round-trip. The C# analog of seeding Python's <c>ServerState.confirm_tools</c>.
    /// </summary>
    private static IReadOnlyList<string> BuildConfirmTools(JsonArray? patterns)
    {
        if (patterns is null)
        {
            return Array.Empty<string>();
        }

        return patterns.Select(p => p!.GetValue<string>()).ToArray();
    }

    /// <summary>
    /// Build an in-memory knowledge base from a scenario's <c>server.knowledge</c> directive — an array
    /// of <c>{ source, content }</c> docs — and wrap it as the (ACL-free) <see cref="IAccessKnowledge"/>
    /// the server retrieves grounding from. Seeding the KB lets the citations dimension run: the server
    /// already populates citations from retrieval (see TurnRunner), so this is purely the runner-side
    /// seed, the analog of the other servers' scenario-knowledge wiring.
    ///
    /// The seeded doc's id is set to its <c>source</c> so the citation is deterministic: the server emits
    /// a citation of <c>(id = DocumentId, title = Source)</c>, so id == title == source makes the
    /// canonical scenario's <c>citations.0.id</c>/<c>.title</c> both resolve to the source — exactly how
    /// the Rust reference made it deterministic.
    ///
    /// Retrieval uses the <see cref="ScenarioKnowledgeBase"/> below rather than the engine's
    /// <c>InMemoryKnowledgeBase</c>: the engine's lexical scorer is EXACT whole-token overlap with no
    /// fallback, whereas the Rust reference scores by SUBSTRING containment ("return" ⊂ "returns") and
    /// the Python reference falls back to the first docs when nothing overlaps — so a canonical scenario
    /// whose user message doesn't share a whole token with the content (e.g. "what is the return policy?"
    /// vs "...returns are accepted...") grounds on Rust/Python but retrieves nothing on the engine's
    /// in-memory base. Matching the reference retrieval keeps the shared corpus at parity.
    /// </summary>
    private static IAccessKnowledge? BuildKnowledge(JsonArray? docs)
    {
        if (docs is null)
        {
            return null;
        }

        var kb = new ScenarioKnowledgeBase();
        foreach (var docNode in docs)
        {
            var doc = docNode!.AsObject();
            var source = doc["source"]!.GetValue<string>();
            var content = doc["content"]!.GetValue<string>();
            // id == source so the citation's id and title both equal the source (deterministic).
            kb.Ingest(source, content);
        }

        return new StaticAccessKnowledge(kb);
    }

    /// <summary>
    /// The runner's seeded knowledge base — a faithful port of the reference servers' in-memory
    /// retrieval so the shared corpus stays at parity across languages. Scores a doc by how many
    /// whitespace-split query words are a SUBSTRING of any of its content words (the Rust reference's
    /// <c>cw.contains(qw)</c> — "return" matches "returns"); when nothing matches it falls back to the
    /// first <c>limit</c> docs scored 0 (the Python reference's no-overlap fallback). Either way a
    /// seeded doc grounds the turn, so the engine populates the <c>citations</c> the scenario asserts.
    /// </summary>
    private sealed class ScenarioKnowledgeBase : IKnowledgeBase
    {
        private readonly List<KnowledgeDocument> _docs = new();

        public void Ingest(string source, string content)
        {
            // id == source so the emitted citation's id and title both equal the source.
            _docs.Add(new KnowledgeDocument(source, content, source));
        }

        public Task IngestAsync(KnowledgeDocument document, CancellationToken cancellationToken = default)
        {
            _docs.Add(document);
            return Task.CompletedTask;
        }

        public Task<IReadOnlyList<KnowledgeResult>> QueryAsync(string query, int limit, CancellationToken cancellationToken = default)
        {
            var queryWords = query.ToLowerInvariant().Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries);

            var scored = _docs
                .Select(doc =>
                {
                    var contentWords = doc.Content.ToLowerInvariant().Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries);
                    // Substring containment, matching the Rust reference (cw.contains(qw)).
                    var matching = queryWords.Count(qw => contentWords.Any(cw => cw.Contains(qw, StringComparison.Ordinal)));
                    var score = contentWords.Length == 0 ? 0.0 : (double)matching / contentWords.Length;
                    return (doc, matching, score);
                })
                .Where(x => x.matching > 0)
                .OrderByDescending(x => x.score)
                .Take(limit)
                .Select(x => new KnowledgeResult(x.doc.Id, x.doc.Content, x.score, x.doc.Source))
                .ToList();

            // No overlap → hand back the first docs (score 0), mirroring the Python reference's fallback
            // so a seeded turn always grounds rather than retrieving nothing.
            IReadOnlyList<KnowledgeResult> hits = scored.Count > 0
                ? scored
                : _docs.Take(limit).Select(doc => new KnowledgeResult(doc.Id, doc.Content, 0.0, doc.Source)).ToList();

            return Task.FromResult(hits);
        }
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

    /// <summary>
    /// Resolve a dotted path (<c>data.data.response.responseParts</c>) into a nested node. A numeric
    /// segment indexes into an array (<c>citations.0.id</c>), so array-element assertions work; a
    /// non-numeric segment indexes into an object.
    /// </summary>
    private static JsonNode? Dot(JsonObject root, string path)
    {
        JsonNode? cur = root;
        foreach (var part in path.Split('.'))
        {
            cur = cur is JsonArray array && int.TryParse(part, out var index)
                ? array[index]
                : cur!.AsObject()[part];
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

    private static WebApplication BuildApp(IChatClient chat, IReadOnlyList<AITool> tools, IReadOnlyList<string> confirmTools, IAccessKnowledge? knowledge)
    {
        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton(chat);
        if (knowledge is not null)
        {
            // Register the scenario's seeded knowledge base so retrieval grounds the turn and the server
            // populates citations (the analog of seeding the other servers' scenario knowledge).
            builder.Services.AddSingleton(knowledge);
        }
        if (tools.Count > 0)
        {
            // Register the scenario's tools as the DI-resolved tool set the WebSocket host threads into
            // each per-connection dispatcher (the analog of seeding Python's ServerState.tools).
            builder.Services.AddSingleton(tools);
        }
        if (confirmTools.Count > 0)
        {
            // Register the scenario's confirmTools so the WebSocket host gates them behind a
            // confirm_tool_action round-trip (the analog of seeding Python's ServerState.confirm_tools).
            builder.Services.AddSingleton(new ConfirmTools(confirmTools));
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
