using System.Runtime.CompilerServices;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// SEP extension host wired into a real server turn: the default-deny allowlist, an extension tool
/// running end-to-end through <c>send_message</c>, and per-agent <c>enabled_tools</c> filtering
/// dropping an extension tool exactly like a native one. Mirrors the Rust
/// <c>tests/sep_extension_host.rs</c>. The live turn spawns the spec's Node echo peer, so it skips
/// cleanly without Node.
/// </summary>
[Collection("SepServerEnv")]
public sealed class SepExtensionHostTests
{
    // ---- ParseAllowlist: the trust gate (default deny), pure ----

    [Fact]
    public void AllowlistParsesCsvAndDeniesByDefault()
    {
        Assert.Empty(ExtensionServerHost.ParseAllowlist(null));
        Assert.Empty(ExtensionServerHost.ParseAllowlist(""));
        Assert.Empty(ExtensionServerHost.ParseAllowlist("  , ,"));
        Assert.Equal(new[] { "todo" }, ExtensionServerHost.ParseAllowlist("todo"));
        Assert.Equal(new[] { "todo", "gate" }, ExtensionServerHost.ParseAllowlist(" todo , gate "));
    }

    [SkippableFact]
    public async Task BuildIsNullWhenAllowlistEmpty()
    {
        using var _ = new EnvScope(("SMOOTH_EXTENSIONS_ALLOW", null), ("SMOOTH_EXTENSIONS_DIR", null));
        var host = await ExtensionServerHost.BuildAsync(_ => { }, "req", "sess", new ConfirmationRegistry());
        Assert.Null(host);
    }

    // ---- the extension tool through a real send_message turn ----

    [SkippableFact]
    public async Task ExtensionToolRunsThroughARealTurn()
    {
        RequireNode();
        var extDir = WriteEchoManifest();
        try
        {
            using var _ = new EnvScope(("SMOOTH_EXTENSIONS_ALLOW", "echo"), ("SMOOTH_EXTENSIONS_DIR", extDir));

            var chat = new ToolThenTextClient("echo.say", new JsonObject { ["phrase"] = "hello from the LLM" }, "done");
            var (dispatcher, events) = Build(chat);
            var sessionId = await CreateSessionAsync(dispatcher, events);

            await dispatcher.DispatchAsync(SendFrame(sessionId, "go"), events.Add);
            await dispatcher.WaitForTurnsAsync();

            // The extension tool executed and echoed the phrase back as a tool-result chunk.
            Assert.Contains(events, e => e["type"]?.GetValue<string>() == "stream_chunk"
                && e.ToJsonString().Contains("hello from the LLM"));

            var terminal = events[^1];
            Assert.Equal("eventual_response", terminal["type"]!.GetValue<string>());
        }
        finally
        {
            Directory.Delete(extDir, recursive: true);
        }
    }

    [SkippableFact]
    public async Task EnabledToolsFilterDropsExtensionToolLikeANativeOne()
    {
        RequireNode();
        var extDir = WriteEchoManifest();
        try
        {
            using var _ = new EnvScope(("SMOOTH_EXTENSIONS_ALLOW", "echo"), ("SMOOTH_EXTENSIONS_DIR", extDir));

            var chat = new ToolThenTextClient("echo.say", new JsonObject { ["phrase"] = "should not run" }, "done");
            // An agent whose enabled_tools list excludes echo.say → it must be filtered out of the
            // turn's tool set exactly like a native tool, so the call resolves to "unknown tool".
            var resolver = new StaticAgentConfigResolver(new AgentConfig(
                EnabledTools: new[] { new EnabledTool("some_native_tool", true, "none", null) }));
            var (dispatcher, events) = Build(chat, resolver);
            var sessionId = await CreateSessionAsync(dispatcher, events);

            await dispatcher.DispatchAsync(SendFrame(sessionId, "go"), events.Add);
            await dispatcher.WaitForTurnsAsync();

            // The ext tool was filtered out of the turn's tool set (exactly like a native tool), so
            // the model's call to echo.say resolved to "unknown tool" — it never executed/echoed. (The
            // tool-CALL chunk still streams its arguments; only the tool RESULT proves execution, and
            // here the only result is the unknown-tool error.)
            var body = string.Join("\n", events.Select(e => e.ToJsonString()));
            Assert.Contains("unknown tool", body);
        }
        finally
        {
            Directory.Delete(extDir, recursive: true);
        }
    }

    // ---- helpers ----

    private static (FrameDispatcher Dispatcher, List<JsonObject> Events) Build(IChatClient chat, IAgentConfigResolver? resolver = null)
    {
        var store = new InMemorySessionStore();
        return (new FrameDispatcher(store, chat, agentConfigResolver: resolver), new List<JsonObject>());
    }

    private static async Task<string> CreateSessionAsync(FrameDispatcher dispatcher, List<JsonObject> events)
    {
        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","requestId":"r1"}""", events.Add);
        var sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
        events.Clear();
        return sessionId;
    }

    private static string SendFrame(string sessionId, string message) =>
        $$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"{{message}}","stream":true}""";

    private static void RequireNode() => Skip.If(NodePath() is null, "node runtime not available");

    private static string? NodePath()
    {
        foreach (var dir in (Environment.GetEnvironmentVariable("PATH") ?? "").Split(Path.PathSeparator))
        {
            var candidate = Path.Combine(dir, "node");
            if (File.Exists(candidate))
            {
                return candidate;
            }
        }
        return null;
    }

    /// <summary>Walk up from the test assembly to the repo's <c>spec/extension/conformance/echo.mjs</c>.</summary>
    private static string SpecEchoPeer()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null)
        {
            var candidate = Path.Combine(dir.FullName, "spec", "extension", "conformance", "echo.mjs");
            if (File.Exists(candidate))
            {
                return candidate;
            }
            dir = dir.Parent;
        }
        throw new FileNotFoundException("could not locate spec/extension/conformance/echo.mjs above the test assembly");
    }

    private static string WriteEchoManifest()
    {
        var node = NodePath()!;
        var peer = SpecEchoPeer().Replace("\\", "\\\\");
        var tmp = Directory.CreateTempSubdirectory("sep-server").FullName;
        var extDir = Path.Combine(tmp, "echo");
        Directory.CreateDirectory(extDir);
        var toml = $"name = \"echo\"\nversion = \"0.1.0\"\n[run]\ncommand = \"{node}\"\nargs = [\"{peer}\"]\n[capabilities]\ntools = true\n";
        File.WriteAllText(Path.Combine(extDir, "extension.toml"), toml);
        return tmp;
    }

    /// <summary>A scripted streaming client: one assistant turn requesting a tool call, then a text turn.</summary>
    private sealed class ToolThenTextClient : IChatClient
    {
        private readonly Queue<ChatResponse> _responses = new();

        public ToolThenTextClient(string toolName, JsonObject arguments, string finalText)
        {
            var args = arguments.ToDictionary(kv => kv.Key, kv => (object?)(kv.Value is JsonValue v && v.TryGetValue<string>(out var s) ? s : kv.Value?.ToJsonString()));
            _responses.Enqueue(new ChatResponse(new ChatMessage(ChatRole.Assistant, new List<AIContent> { new FunctionCallContent("c1", toolName, args) }))
            {
                Usage = new UsageDetails { InputTokenCount = 1, OutputTokenCount = 1, TotalTokenCount = 2 },
                ModelId = "mock",
            });
            _responses.Enqueue(new ChatResponse(new ChatMessage(ChatRole.Assistant, finalText)) { ModelId = "mock" });
        }

        private ChatResponse Next() =>
            _responses.Count > 0 ? _responses.Dequeue() : new ChatResponse(new ChatMessage(ChatRole.Assistant, string.Empty));

        public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default) =>
            Task.FromResult(Next());

        public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
            IEnumerable<ChatMessage> messages, ChatOptions? options = null, [EnumeratorCancellation] CancellationToken cancellationToken = default)
        {
            foreach (var update in Next().ToChatResponseUpdates())
            {
                await Task.Yield();
                yield return update;
            }
        }

        public object? GetService(Type serviceType, object? serviceKey = null) => null;

        public void Dispose() { }
    }

    private sealed class StaticAgentConfigResolver : IAgentConfigResolver
    {
        private readonly AgentConfig _config;
        public StaticAgentConfigResolver(AgentConfig config) => _config = config;
        public Task<AgentConfig?> ResolveAsync(string agentId, CancellationToken cancellationToken = default) => Task.FromResult<AgentConfig?>(_config);
    }

    /// <summary>Sets env vars for the test's duration, restoring the prior values on dispose.</summary>
    private sealed class EnvScope : IDisposable
    {
        private readonly (string Key, string? Prior)[] _prior;

        public EnvScope(params (string Key, string? Value)[] vars)
        {
            _prior = vars.Select(v => (v.Key, Environment.GetEnvironmentVariable(v.Key))).ToArray();
            foreach (var (key, value) in vars)
            {
                Environment.SetEnvironmentVariable(key, value);
            }
        }

        public void Dispose()
        {
            foreach (var (key, prior) in _prior)
            {
                Environment.SetEnvironmentVariable(key, prior);
            }
        }
    }
}

/// <summary>Serializes the SEP server tests that mutate <c>SMOOTH_EXTENSIONS_*</c> process env.</summary>
[CollectionDefinition("SepServerEnv", DisableParallelization = true)]
public sealed class SepServerEnvCollection { }
