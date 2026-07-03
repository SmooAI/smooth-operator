using System.Text.Json.Nodes;
using SmooAI.SmoothOperator.Core.Extensions;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// SEP extension hosting for the C# operator server. Discovers <c>extension.toml</c> extensions,
/// spawns them as JSON-RPC/ndjson subprocesses, and exposes their tools so a server-side turn can
/// register them into the agent's tool set (composing with per-agent <c>enabled_tools</c> filtering).
/// Mirrors the Rust reference <c>smooth-operator-server/src/extensions.rs</c>.
///
/// <para><b>Trust — default deny.</b> The server has no interactive trust prompt, so
/// <c>SMOOTH_EXTENSIONS_ALLOW</c> (comma-separated extension names) IS the trust decision: empty (the
/// default) ⇒ no extension is ever spawned and no host is built, so behavior is byte-for-byte
/// unchanged.</para>
///
/// <para><b><c>ui/confirm</c> → the confirmation frame.</b> <see cref="ConfirmUiProvider"/> projects an
/// extension's <c>ui/confirm</c> onto the operator protocol's
/// <c>write_confirmation_required</c>/<c>confirm_tool_action</c> frames — the same
/// <see cref="ConfirmationRegistry"/> the native write-tool HITL parks on. Every other <c>ui/*</c>
/// degrades headless.</para>
/// </summary>
public static class ExtensionServerHost
{
    /// <summary>Frontend <c>mode</c> announced to extensions at handshake. The servers front the
    /// chat-widget, whose confirm lands as chat-native button frames.</summary>
    private const string UiMode = "widget";

    /// <summary>Parse <c>SMOOTH_EXTENSIONS_ALLOW</c> into the set of allowed extension names
    /// (comma-separated, trimmed, empties dropped). Absent/blank ⇒ empty ⇒ deny all.</summary>
    public static List<string> ParseAllowlist(string? raw) =>
        (raw ?? string.Empty).Split(',').Select(s => s.Trim()).Where(s => s.Length > 0).ToList();

    /// <summary>
    /// Discover, trust-gate (allowlist), and load the per-turn extension host for a session's turn.
    /// Returns null — the host is never built, zero overhead — when the allowlist is empty (default
    /// deny) or no allowed extension loads. The caller registers <see cref="ExtensionHost.Tools"/>
    /// into the turn's tool set and calls <see cref="ExtensionHost.ShutdownAllAsync"/> at turn end.
    /// </summary>
    public static async Task<ExtensionHost?> BuildAsync(Action<JsonObject> sink, string requestId, string sessionId, ConfirmationRegistry confirmations)
    {
        // Trust = a default-deny env allowlist (the server has no interactive prompt).
        var allow = ParseAllowlist(Environment.GetEnvironmentVariable("SMOOTH_EXTENSIONS_ALLOW"));
        if (allow.Count == 0)
        {
            return null; // default deny — never spawn anything
        }

        // SMOOTH_EXTENSIONS_DIR overrides the discovery dir; else the engine default
        // ($SMOOTH_HOME/extensions or ~/.smooth/extensions).
        var dirOverride = Environment.GetEnvironmentVariable("SMOOTH_EXTENSIONS_DIR")?.Trim();
        var global = string.IsNullOrEmpty(dirOverride) ? ExtensionDiscovery.DefaultGlobalDir() : dirOverride;
        // The server has no per-session workspace; project-scoped discovery keys off the process cwd.
        var project = ExtensionDiscovery.ProjectDir(Directory.GetCurrentDirectory());

        var (discovered, discFailures) = ExtensionDiscovery.Discover(global, project);
        foreach (var (src, err) in discFailures)
        {
            System.Diagnostics.Debug.WriteLine($"sep: extension manifest failed to parse: {src}: {err}");
        }

        var allowed = discovered.Where(e => allow.Contains(e.Manifest.Name)).ToList();
        if (allowed.Count == 0)
        {
            return null;
        }

        var hostInfo = new HostInfo { Name = "smooth-operator-server", Version = "1.0.0" };
        // Allowlisted ⇒ trusted (the allowlist is the trust decision); project-scoped extensions load
        // because `trusted` is true.
        var workspace = new WorkspaceInfo { Root = Directory.GetCurrentDirectory(), Trusted = true };
        var @delegate = new ConfirmUiProvider(sink, requestId, sessionId, confirmations);

        var (host, loadFailures) = await ExtensionHost.LoadAsync(allowed, hostInfo, workspace, UiMode, new List<string> { "confirm" }, @delegate).ConfigureAwait(false);
        foreach (var (name, err) in loadFailures)
        {
            System.Diagnostics.Debug.WriteLine($"sep: extension failed to load: {name}: {err}");
        }
        return host.IsEmpty ? null : host;
    }
}

/// <summary>
/// The <see cref="HostDelegate"/> that bridges an extension's <c>ui/confirm</c> onto the operator
/// protocol's <c>write_confirmation_required</c> frame and degrades every other <c>ui/*</c> headless.
/// Bound to ONE turn (its sink, request id, session) — which is why the host is built per turn.
/// Mirrors the Rust <c>ConfirmUiProvider</c>.
/// </summary>
internal sealed class ConfirmUiProvider : HostDelegate
{
    private readonly Action<JsonObject> _sink;
    private readonly string _requestId;
    private readonly string _sessionId;
    private readonly ConfirmationRegistry _confirmations;

    public ConfirmUiProvider(Action<JsonObject> sink, string requestId, string sessionId, ConfirmationRegistry confirmations)
    {
        _sink = sink;
        _requestId = requestId;
        _sessionId = sessionId;
        _confirmations = confirmations;
    }

    public override async Task<JsonNode> UiRequestAsync(string ext, JsonNode @params)
    {
        var kind = @params["kind"]?.GetValue<string>() ?? string.Empty;
        switch (kind)
        {
            case "confirm":
                var prompt = @params["prompt"]?.GetValue<string>() ?? "Confirm this action?";
                // Register a fresh responder for this session so the next inbound confirm_tool_action
                // resumes THIS request, then emit the frame and park until the human answers. The
                // registry keys by session; the turn's finally clears it. One confirm resolves one park.
                var pending = _confirmations.Register(_sessionId);
                _sink(ProtocolEvents.WriteConfirmationRequired(_requestId, ext, prompt));
                var approved = await pending.ConfigureAwait(false);
                return new JsonObject { ["confirmed"] = approved };
            // Render-only kinds: accept and drop — there's no chat frame for them.
            case "notify":
            case "set_status":
            case "set_widget":
            case "set_title":
                return new JsonObject();
            // select/input need an answer a confirm button can't source.
            default:
                return new JsonObject { ["cancelled"] = true };
        }
    }
}
