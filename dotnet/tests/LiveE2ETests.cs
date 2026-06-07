// Live LLM WebSocket E2E — gated on `SMOOTH_AGENT_E2E=1` + `SMOOAI_GATEWAY_KEY`.
//
// This is a REAL end-to-end test: it boots the actual Rust WS service
// (smooth-operator-agent-server) as a child process with a seeded knowledge base
// and a real gateway key, then drives a full streaming turn through the real
// SmoothAgentClient over a real ClientWebSocket (WebSocketTransport) and asserts:
//
//   1. knowledge grounding — `send_message("What is SmooAI's return window?…")`
//      streams ≥1 `stream_token`/`stream_chunk` and the terminal
//      `eventual_response` reply contains "17" (the seeded "17 days" fact).
//   2. per-session memory — within the SAME session, "My name is Zog. Remember it."
//      then "What is my name?" → the reply contains "Zog".
//
// ## Gating (safe in CI without creds)
//
// The test is SKIPPED (not failed) unless BOTH are set:
//   - `SMOOTH_AGENT_E2E=1`
//   - `SMOOAI_GATEWAY_KEY=<key>` (never printed)
//
// `dotnet test` with neither set keeps the suite at 21 passed + this one skipped.
// The skip is modeled with Xunit.SkippableFact's `Skip.If(...)` so it shows up as
// a real "Skipped" result, not a silent pass.
//
// ## Run locally (does not print the key)
//
//   export SMOOAI_GATEWAY_KEY=$(python3 -c \
//     "import json;print(json.load(open('$HOME/.local/share/opencode/auth.json'))['smooai']['key'])")
//   export SMOOTH_AGENT_E2E=1
//   dotnet test --filter LiveE2E --logger "console;verbosity=detailed"

using System.Diagnostics;
using System.Net.Sockets;
using System.Text.Json;
using Xunit.Abstractions;

namespace SmooAI.SmoothOperatorAgent.Tests;

public sealed class LiveE2ETests
{
    private const int Port = 8812;
    private const string WsUrl = "ws://127.0.0.1:8812/ws";
    private const string Model = "claude-haiku-4-5";

    // The pre-built debug binary. If absent the test is skipped (it is not the
    // test's job to compile Rust).
    private static readonly string ServerBinary =
        Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.UserProfile),
            ".cargo", "shared-target", "debug", "smooth-operator-agent-server");

    // Generous per-turn budget: the live gateway + tool loop can be slow.
    private static readonly TimeSpan TurnTimeout = TimeSpan.FromSeconds(120);

    private readonly ITestOutputHelper _out;

    public LiveE2ETests(ITestOutputHelper output) => _out = output;

    [SkippableFact]
    public async Task Live_KnowledgeGrounded_And_SessionMemory()
    {
        // ── Gate: skip (never fail) without explicit opt-in + a key. ──
        Skip.IfNot(
            Environment.GetEnvironmentVariable("SMOOTH_AGENT_E2E") == "1",
            "SMOOTH_AGENT_E2E != \"1\" — skipping live-gateway WS test.");

        var gatewayKey = Environment.GetEnvironmentVariable("SMOOAI_GATEWAY_KEY");
        Skip.If(
            string.IsNullOrWhiteSpace(gatewayKey),
            "SMOOAI_GATEWAY_KEY unset/empty — skipping live-gateway WS test.");

        Skip.IfNot(
            File.Exists(ServerBinary),
            $"server binary not built at {ServerBinary} — run `cargo build -p smooai-smooth-operator-agent-server --bin smooth-operator-agent-server`.");

        // ── Boot the real Rust service as a child process. ──
        await using var server = await ServerProcess.StartAsync(ServerBinary, Port, Model, gatewayKey!, _out);

        // ── Connect the real client over a real WebSocket. ──
        await using var client = new SmoothAgentClient(new SmoothAgentClientOptions
        {
            Url = WsUrl,
            RequestTimeout = TurnTimeout,
        });
        await client.ConnectAsync();

        // 1. Create a session.
        var session = await client.CreateConversationSessionAsync(new CreateConversationSessionAction
        {
            AgentId = "e2e",
            UserName = "Zog E2E",
        });
        Assert.False(string.IsNullOrEmpty(session.SessionId), "session creation must return a non-empty sessionId");
        _out.WriteLine($"[live-ws] session: {session.SessionId}");

        // 2. Turn 1 — knowledge-grounded ("17"-day return window).
        var (events1, eventual1) = await RunTurnAsync(
            client, session.SessionId, "What is SmooAI's return window? Search the knowledge base.");

        var tokenCount = events1.OfType<StreamTokenEvent>().Count();
        var chunkCount = events1.OfType<StreamChunkEvent>().Count();
        _out.WriteLine($"[live-ws] turn 1 streamed: {tokenCount} stream_token, {chunkCount} stream_chunk events");

        var sample = string.Concat(events1.OfType<StreamTokenEvent>().Select(e => e.Token).Take(40));
        _out.WriteLine($"[live-ws] turn 1 token sample: {sample}");

        var reply1 = FinalText(eventual1);
        _out.WriteLine($"[live-ws] turn 1 final reply: {reply1}");

        Assert.True(
            tokenCount + chunkCount >= 1,
            "expected at least one streamed stream_token or stream_chunk event in turn 1");
        Assert.Equal(200, eventual1.Status);
        Assert.False(
            string.IsNullOrEmpty(eventual1.Data.Payload.MessageId),
            "eventual_response must carry a messageId");
        Assert.Contains("17", reply1);

        // 3. Turns 2 + 3 — per-session memory ("Zog"), same session.
        var (_, eventual2) = await RunTurnAsync(
            client, session.SessionId, "My name is Zog. Remember it.");
        _out.WriteLine($"[live-ws] turn 2 reply: {FinalText(eventual2)}");

        var (_, eventual3) = await RunTurnAsync(
            client, session.SessionId, "What is my name? Reply with just the name.");
        var reply3 = FinalText(eventual3);
        _out.WriteLine($"[live-ws] turn 3 reply (memory check): {reply3}");

        Assert.Contains("Zog", reply3, StringComparison.OrdinalIgnoreCase);
    }

    /// <summary>
    /// Drive one streaming turn through the real client: collect every event in
    /// arrival order via async-iteration, and await the terminal eventual_response.
    /// </summary>
    private static async Task<(List<ServerEvent> Events, EventualResponseEvent Eventual)> RunTurnAsync(
        SmoothAgentClient client, string sessionId, string message)
    {
        var turn = client.SendMessageAsync(new SendMessageAction
        {
            SessionId = sessionId,
            Message = message,
        });

        var events = new List<ServerEvent>();
        using var cts = new CancellationTokenSource(TurnTimeout);
        await foreach (var ev in turn.WithCancellation(cts.Token))
            events.Add(ev);

        var eventual = await turn.Completion;
        return (events, eventual);
    }

    /// <summary>
    /// Extract the assistant text from an eventual_response. The runner puts the
    /// reply in <c>data.data.response.responseParts[]</c> (or a bare string).
    /// </summary>
    private static string FinalText(EventualResponseEvent eventual)
    {
        if (eventual.Data.Payload.Response is not { } resp)
            return string.Empty;

        if (resp.ValueKind == JsonValueKind.String)
            return resp.GetString() ?? string.Empty;

        if (resp.ValueKind == JsonValueKind.Object &&
            resp.TryGetProperty("responseParts", out var parts) &&
            parts.ValueKind == JsonValueKind.Array)
        {
            return string.Join(" ",
                parts.EnumerateArray()
                    .Where(p => p.ValueKind == JsonValueKind.String)
                    .Select(p => p.GetString()));
        }

        return resp.ToString();
    }
}

/// <summary>
/// Spawns the smooth-operator-agent-server child process with the live config,
/// waits until its TCP port accepts connections, and kills it on disposal.
/// The gateway key is passed only via the child's environment — never logged.
/// </summary>
internal sealed class ServerProcess : IAsyncDisposable
{
    private readonly Process _process;

    private ServerProcess(Process process) => _process = process;

    public static async Task<ServerProcess> StartAsync(
        string binary, int port, string model, string gatewayKey, ITestOutputHelper output)
    {
        var psi = new ProcessStartInfo
        {
            FileName = binary,
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
        };
        psi.Environment["SMOOTH_AGENT_PORT"] = port.ToString();
        psi.Environment["SMOOTH_AGENT_SEED_KB"] = "1";
        psi.Environment["SMOOTH_AGENT_MODEL"] = model;
        psi.Environment["SMOOAI_GATEWAY_KEY"] = gatewayKey; // never logged

        var process = new Process { StartInfo = psi, EnableRaisingEvents = true };

        // Surface server logs into the test output (the key is never logged by the
        // server's own "listening" line, which only prints host/model).
        process.OutputDataReceived += (_, e) => { if (e.Data is not null) output.WriteLine($"[server] {e.Data}"); };
        process.ErrorDataReceived += (_, e) => { if (e.Data is not null) output.WriteLine($"[server] {e.Data}"); };

        if (!process.Start())
            throw new InvalidOperationException($"failed to start server binary: {binary}");
        process.BeginOutputReadLine();
        process.BeginErrorReadLine();

        var server = new ServerProcess(process);
        try
        {
            await WaitForPortAsync(port, TimeSpan.FromSeconds(30), process);
            output.WriteLine($"[live-ws] server ready on 127.0.0.1:{port}");
        }
        catch
        {
            await server.DisposeAsync();
            throw;
        }
        return server;
    }

    private static async Task WaitForPortAsync(int port, TimeSpan timeout, Process process)
    {
        var deadline = DateTime.UtcNow + timeout;
        while (DateTime.UtcNow < deadline)
        {
            if (process.HasExited)
                throw new InvalidOperationException($"server exited early with code {process.ExitCode}");

            try
            {
                using var tcp = new TcpClient();
                using var connectCts = new CancellationTokenSource(TimeSpan.FromSeconds(1));
                await tcp.ConnectAsync(System.Net.IPAddress.Loopback, port, connectCts.Token);
                return; // port accepted a connection — server is up
            }
            catch (Exception ex) when (ex is SocketException or OperationCanceledException)
            {
                await Task.Delay(200);
            }
        }
        throw new TimeoutException($"server did not open port {port} within {timeout.TotalSeconds}s");
    }

    public async ValueTask DisposeAsync()
    {
        try
        {
            if (!_process.HasExited)
            {
                _process.Kill(entireProcessTree: true);
                await _process.WaitForExitAsync(new CancellationTokenSource(TimeSpan.FromSeconds(5)).Token);
            }
        }
        catch
        {
            // Best-effort teardown.
        }
        finally
        {
            _process.Dispose();
        }
    }
}
