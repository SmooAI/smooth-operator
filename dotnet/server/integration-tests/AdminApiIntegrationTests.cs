using System.Net;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.TestHost;
using Microsoft.Extensions.DependencyInjection;
using SmooAI.SmoothOperator.Server.AspNetCore;

namespace SmooAI.SmoothOperator.Server.IntegrationTests;

/// <summary>
/// End-to-end tests for the auth-gated admin HTTP API: /admin/health is ungated, everything else is
/// fail-closed (401 without a resolvable identity), and /admin/reindex actually drives the
/// ingestion service. Boots the host in-process (TestServer) and hits it over real HTTP. CI-safe
/// (a MockConnector — no GitHub).
/// </summary>
public class AdminApiIntegrationTests
{
    private static WebApplication BuildAdminApp(AuthMode mode, RepoIngestionService? ingestion)
    {
        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton(new TokenAccessResolver(new AuthOptions { Mode = mode }));
        if (ingestion is not null)
        {
            builder.Services.AddSingleton(ingestion);
        }

        var app = builder.Build();
        app.MapSmoothOperatorAdmin();
        return app;
    }

    private static string TrustedToken(object claims) =>
        Convert.ToBase64String(Encoding.UTF8.GetBytes(JsonSerializer.Serialize(claims))).TrimEnd('=').Replace('+', '-').Replace('/', '_');

    [Fact]
    public async Task Health_IsUngated()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted, ingestion: null);
        await app.StartAsync();
        var response = await app.GetTestServer().CreateClient().GetAsync("/admin/health");
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        await app.StopAsync();
    }

    [Fact]
    public async Task Me_FailsClosed_WithoutToken_AndReturnsIdentity_WithToken()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted, ingestion: null);
        await app.StartAsync();
        var client = app.GetTestServer().CreateClient();

        var anonymous = await client.GetAsync("/admin/me");
        Assert.Equal(HttpStatusCode.Unauthorized, anonymous.StatusCode);

        var token = TrustedToken(new { sub = "u1", org = "acme", role = "admin", groups = new[] { "github:acme/docs" } });
        var request = new HttpRequestMessage(HttpMethod.Get, "/admin/me");
        request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
        var authed = await client.SendAsync(request);

        Assert.Equal(HttpStatusCode.OK, authed.StatusCode);
        var body = await authed.Content.ReadAsStringAsync();
        Assert.Contains("u1", body);
        Assert.Contains("acme", body);

        await app.StopAsync();
    }

    [Fact]
    public async Task Reindex_FailsClosed_WithoutToken_AndRunsIngestion_WithToken()
    {
        var kb = new AclKnowledgeStore();
        var ingestion = new RepoIngestionService(
            new[] { new RepoSpec("acme", "docs", "main") },
            kb,
            _ => new MockConnector(new SourceDocument("d1", "runbook.md", "Deploy steps live here.")));

        await using var app = BuildAdminApp(AuthMode.Trusted, ingestion);
        await app.StartAsync();
        var client = app.GetTestServer().CreateClient();

        var anonymous = await client.PostAsync("/admin/reindex", content: null);
        Assert.Equal(HttpStatusCode.Unauthorized, anonymous.StatusCode);

        var token = TrustedToken(new { sub = "u1", org = "acme", role = "admin", groups = Array.Empty<string>() });
        var request = new HttpRequestMessage(HttpMethod.Post, "/admin/reindex");
        request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
        var authed = await client.SendAsync(request);

        Assert.Equal(HttpStatusCode.OK, authed.StatusCode);
        var body = await authed.Content.ReadAsStringAsync();
        Assert.Contains("acme/docs", body); // a result row for the configured repo

        await app.StopAsync();
    }

    [Fact]
    public async Task Connectors_ListsConfiguredRepos_WhenAuthed()
    {
        var ingestion = new RepoIngestionService(
            new[] { new RepoSpec("acme", "docs", "main") },
            new AclKnowledgeStore(),
            _ => new MockConnector());

        await using var app = BuildAdminApp(AuthMode.Trusted, ingestion);
        await app.StartAsync();
        var client = app.GetTestServer().CreateClient();

        var token = TrustedToken(new { sub = "u1", org = "acme", role = "admin", groups = Array.Empty<string>() });
        var request = new HttpRequestMessage(HttpMethod.Get, "/admin/connectors");
        request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
        var response = await client.SendAsync(request);

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var body = await response.Content.ReadAsStringAsync();
        Assert.Contains("acme/docs", body);
        Assert.Contains("github:acme/docs", body);

        await app.StopAsync();
    }
}
