namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// The env contract this host reads, resolved canonical-first.
/// </summary>
/// <remarks>
/// <para>
/// Every server implementation (Rust, Go, Python, TypeScript, .NET) reads the same
/// canonical <c>SMOOTH_AGENT_*</c> names. Each host's PRE-PARITY name is kept as an
/// alias so no existing deployment breaks; the canonical name wins when both are set.
/// </para>
/// <para>
/// This host's aliases: <c>SMOOTH_DATABASE_URL</c>, <c>SMOOTH_AUTH_MODE</c>,
/// <c>SMOOTH_MODEL</c> / <c>SMOOAI_MODEL</c>, <c>SMOOTH_MAX_TOKENS</c>,
/// <c>SMOOTH_MAX_ITERATIONS</c>. The gateway triple keeps its <c>SMOOAI_*</c> spelling —
/// that name is already identical across all five hosts and is the wider SmooAI gateway
/// contract, not this server's own config surface (th-df7007 is why: this host once read
/// only <c>SMOOTH_GATEWAY_KEY</c> while every launcher exported <c>SMOOAI_GATEWAY_KEY</c>,
/// so every turn 401'd).
/// </para>
/// </remarks>
public static class ServerEnv
{
    /// <summary>Default bind host, shared with the sibling hosts.</summary>
    public const string DefaultHost = "127.0.0.1";

    /// <summary>Default bind port, shared with the sibling hosts.</summary>
    public const int DefaultPort = 8787;

    /// <summary>The first of <paramref name="values"/> that is non-null and non-blank, else "".</summary>
    public static string First(params string?[] values) =>
        Array.Find(values, v => !string.IsNullOrWhiteSpace(v))?.Trim() ?? string.Empty;

    /// <summary>
    /// The ASP.NET listen URL this host should bind, or <c>null</c> to leave whatever
    /// <c>ASPNETCORE_URLS</c> already configures alone.
    /// </summary>
    /// <remarks>
    /// Before the env-parity pass this host had NO bind/port env of its own — it took
    /// ASP.NET's default (:5000), which is neither of the ports its four siblings serve.
    /// So: an explicit <c>SMOOTH_AGENT_BIND</c>/<c>_PORT</c> wins, an explicit
    /// <c>ASPNETCORE_URLS</c> is left untouched (it is what the container image sets), and
    /// with neither configured the host falls in line on <c>127.0.0.1:8787</c>.
    /// </remarks>
    public static string? ResolveUrls(Func<string, string?> get)
    {
        var bind = First(get("SMOOTH_AGENT_BIND"));
        var port = First(get("SMOOTH_AGENT_PORT"));
        var aspnetUrls = First(get("ASPNETCORE_URLS"));

        if (bind.Length == 0 && port.Length == 0 && aspnetUrls.Length > 0)
        {
            return null;
        }

        // Overriding ONLY the port must not silently narrow the bind: a container whose
        // ASPNETCORE_URLS listens on every interface would become unreachable the moment
        // someone moved its port with the canonical name. So the host half defaults to the
        // wildcard whenever ASPNETCORE_URLS is what we are replacing, and to loopback
        // (matching every sibling host) when nothing was configured at all.
        var host = bind.Length > 0 ? bind : aspnetUrls.Length > 0 ? "+" : DefaultHost;
        // ASP.NET spells "every interface" as + or *, not 0.0.0.0; binding the literal
        // 0.0.0.0 throws at startup. The chart and every sibling Dockerfile set 0.0.0.0.
        if (host is "0.0.0.0" or "::" or "[::]")
        {
            host = "+";
        }

        var resolvedPort = int.TryParse(port, out var p) && p > 0 ? p : DefaultPort;
        return $"http://{host}:{resolvedPort}";
    }
}
