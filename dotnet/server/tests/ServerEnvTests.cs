using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// The env-parity contract: every server implementation reads the canonical
/// <c>SMOOTH_AGENT_*</c> names, and each one's pre-parity name keeps working as an
/// alias so no existing deployment breaks. Canonical wins when both are set.
/// </summary>
public class ServerEnvTests
{
    private static Func<string, string?> Env(params (string Key, string Value)[] pairs)
    {
        var map = pairs.ToDictionary(p => p.Key, p => p.Value);
        return key => map.TryGetValue(key, out var value) ? value : null;
    }

    [Fact]
    public void First_prefers_the_canonical_name_over_its_aliases()
    {
        Assert.Equal("canonical", ServerEnv.First("canonical", "alias"));
        Assert.Equal("alias", ServerEnv.First(null, "alias"));
        Assert.Equal("alias", ServerEnv.First("   ", "alias"));
        Assert.Equal(string.Empty, ServerEnv.First(null, null));
        Assert.Equal("trimmed", ServerEnv.First("  trimmed  "));
    }

    [Fact]
    public void ResolveUrls_falls_in_line_with_the_sibling_hosts_when_nothing_is_configured()
    {
        // Pre-parity this host had no bind env and took ASP.NET's :5000 default.
        Assert.Equal("http://127.0.0.1:8787", ServerEnv.ResolveUrls(Env()));
    }

    [Fact]
    public void ResolveUrls_reads_the_canonical_bind_and_port()
    {
        var urls = ServerEnv.ResolveUrls(Env(("SMOOTH_AGENT_BIND", "127.0.0.1"), ("SMOOTH_AGENT_PORT", "9000")));
        Assert.Equal("http://127.0.0.1:9000", urls);
    }

    [Fact]
    public void ResolveUrls_leaves_an_explicit_ASPNETCORE_URLS_alone()
    {
        // The container image sets ASPNETCORE_URLS; only a SMOOTH_AGENT_* bind overrides it.
        Assert.Null(ServerEnv.ResolveUrls(Env(("ASPNETCORE_URLS", "http://+:8080"))));
    }

    [Fact]
    public void ResolveUrls_canonical_bind_wins_over_ASPNETCORE_URLS()
    {
        var urls = ServerEnv.ResolveUrls(Env(("ASPNETCORE_URLS", "http://+:8080"), ("SMOOTH_AGENT_BIND", "0.0.0.0"), ("SMOOTH_AGENT_PORT", "9000")));
        Assert.Equal("http://+:9000", urls);
    }

    [Fact]
    public void ResolveUrls_moving_only_the_port_does_not_narrow_a_container_bind()
    {
        // Defaulting the host half to loopback here would make a container that had been
        // listening on every interface unreachable the moment someone moved its port.
        var urls = ServerEnv.ResolveUrls(Env(("ASPNETCORE_URLS", "http://+:8080"), ("SMOOTH_AGENT_PORT", "9000")));
        Assert.Equal("http://+:9000", urls);
    }

    [Theory]
    [InlineData("0.0.0.0")]
    [InlineData("::")]
    [InlineData("[::]")]
    public void ResolveUrls_translates_wildcard_binds_to_the_ASPNET_spelling(string bind)
    {
        // ASP.NET spells "every interface" as +; binding the literal 0.0.0.0 throws at
        // startup, and 0.0.0.0 is exactly what the chart and sibling Dockerfiles set.
        Assert.Equal("http://+:8787", ServerEnv.ResolveUrls(Env(("SMOOTH_AGENT_BIND", bind))));
    }

    [Fact]
    public void ResolveUrls_falls_back_to_the_default_port_rather_than_binding_garbage()
    {
        Assert.Equal("http://+:8787", ServerEnv.ResolveUrls(Env(("SMOOTH_AGENT_BIND", "0.0.0.0"), ("SMOOTH_AGENT_PORT", "not-a-port"))));
    }
}
