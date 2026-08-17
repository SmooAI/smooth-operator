using System.Net;
using System.Net.Http.Headers;
using System.Net.Http.Json;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.TestHost;
using Microsoft.Extensions.DependencyInjection;
using SmooAI.SmoothOperator.Server.AspNetCore;

namespace SmooAI.SmoothOperator.Server.IntegrationTests;

/// <summary>
/// The <c>/admin/*</c> API the console drives. Two things matter per route: it must fail CLOSED
/// without a sufficient token, and it must answer the wire shape the console's typed client expects
/// (camelCase, <c>{"error":{"code","message"}}</c>) — the same contract the Rust, Go, TypeScript and
/// Python servers ship.
/// <para>
/// Driven over real HTTP against a booted host (TestServer), so the routing, the auth gate and the
/// JSON all have to actually work together. CI-safe: no GitHub, no model, no database.
/// </para>
/// </summary>
public class AdminApiIntegrationTests
{
    /// <summary>Every gated route with the minimum role it requires — the contract table.</summary>
    public static TheoryData<string, string> GatedRoutes() => new()
    {
        { "GET", "/admin/me" },
        { "GET", "/admin/conversations" },
        { "GET", "/admin/conversations/c1/messages" },
        { "GET", "/admin/indexing/runs" },
        { "GET", "/admin/document-sets" },
        { "GET", "/admin/connectors" },
        { "POST", "/admin/connectors" },
        { "GET", "/admin/connectors/x" },
        { "PUT", "/admin/connectors/x" },
        { "DELETE", "/admin/connectors/x" },
        { "POST", "/admin/connectors/x/index" },
        { "GET", "/admin/settings" },
        { "PUT", "/admin/settings" },
        { "POST", "/admin/publish" },
        { "POST", "/admin/reindex" },
    };

    private static WebApplication BuildAdminApp(
        AuthMode mode,
        RepoIngestionService? ingestion = null,
        ISessionStore? store = null,
        IBackplane? backplane = null)
    {
        var builder = WebApplication.CreateBuilder();
        builder.WebHost.UseTestServer();
        builder.Services.AddSingleton(new TokenAccessResolver(new AuthOptions { Mode = mode }));
        if (ingestion is not null)
        {
            builder.Services.AddSingleton(ingestion);
        }
        if (store is not null)
        {
            builder.Services.AddSingleton(store);
        }
        if (backplane is not null)
        {
            builder.Services.AddSingleton(backplane);
        }

        var app = builder.Build();
        app.MapSmoothOperatorAdmin();
        return app;
    }

    /// <summary>
    /// A token for the <c>trusted</c> verifier: base64url(JSON claims), so a test picks a role by
    /// naming it. Anything that is not decodable JSON (e.g. "garbage") fails closed to anonymous.
    /// </summary>
    private static string Token(string sub, string org, string role, string? email = null) =>
        Convert.ToBase64String(Encoding.UTF8.GetBytes(JsonSerializer.Serialize(new { sub, org, role, email })))
            .TrimEnd('=').Replace('+', '-').Replace('/', '_');

    private static string Admin(string org = "org-1") => Token("u-admin", org, "admin");

    private static string Curator(string org = "org-1") => Token("u-curator", org, "curator");

    private static string Basic(string org = "org-1") => Token("u-basic", org, "basic");

    private static async Task<(HttpStatusCode Status, JsonNode? Json)> Call(
        WebApplication app, string method, string path, string? token = null, object? body = null)
    {
        var request = new HttpRequestMessage(new HttpMethod(method), path);
        if (token is not null)
        {
            request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
        }
        if (body is not null)
        {
            request.Content = JsonContent.Create(body);
        }
        var response = await app.GetTestServer().CreateClient().SendAsync(request);
        var text = await response.Content.ReadAsStringAsync();
        return (response.StatusCode, string.IsNullOrEmpty(text) ? null : JsonNode.Parse(text));
    }

    // ── auth gate ───────────────────────────────────────────────────────────────

    [Fact]
    public async Task Health_IsUngated()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();
        var (status, json) = await Call(app, "GET", "/admin/health");
        Assert.Equal(HttpStatusCode.OK, status);
        Assert.Equal("ok", json!["status"]!.GetValue<string>());
        await app.StopAsync();
    }

    [Theory]
    [MemberData(nameof(GatedRoutes))]
    public async Task EveryGatedRoute_FailsClosed_WithoutAToken(string method, string path)
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();
        var (status, json) = await Call(app, method, path, token: null, body: new { });
        Assert.Equal(HttpStatusCode.Unauthorized, status);
        Assert.Equal("UNAUTHENTICATED", json!["error"]!["code"]!.GetValue<string>());
        await app.StopAsync();
    }

    [Fact]
    public async Task InvalidToken_Is401_NotAnAnonymousGrant()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();
        var (status, json) = await Call(app, "GET", "/admin/me", "garbage");
        Assert.Equal(HttpStatusCode.Unauthorized, status);
        Assert.Equal("INVALID_TOKEN", json!["error"]!["code"]!.GetValue<string>());
        await app.StopAsync();
    }

    [Fact]
    public async Task EnforcesRoleRank_InBothDirections()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();

        Assert.Equal(HttpStatusCode.OK, (await Call(app, "GET", "/admin/me", Basic())).Status);
        Assert.Equal(HttpStatusCode.Forbidden, (await Call(app, "GET", "/admin/settings", Basic())).Status);
        Assert.Equal(HttpStatusCode.OK, (await Call(app, "GET", "/admin/settings", Curator())).Status);

        var denied = await Call(app, "PUT", "/admin/settings", Curator(), new { model = "m" });
        Assert.Equal(HttpStatusCode.Forbidden, denied.Status);
        Assert.Equal("FORBIDDEN", denied.Json!["error"]!["code"]!.GetValue<string>());

        await app.StopAsync();
    }

    /// <summary>
    /// AUTH_MODE=none must resolve to an ADMIN principal, or the console 403-walls against a local
    /// server — as useless as the 404s this API exists to remove. Both directions are asserted: the
    /// grant applies on a no-auth server, and never leaks into an auth-enabled one.
    /// </summary>
    [Fact]
    public async Task NoAuthMode_GrantsAdmin_ButStillNeedsSomeToken()
    {
        await using var dev = BuildAdminApp(AuthMode.None);
        await dev.StartAsync();

        foreach (var path in new[] { "/admin/settings", "/admin/connectors", "/admin/indexing/runs", "/admin/document-sets" })
        {
            Assert.Equal(HttpStatusCode.OK, (await Call(dev, "GET", path, "dev")).Status);
        }
        Assert.Equal(HttpStatusCode.OK, (await Call(dev, "PUT", "/admin/settings", "dev", new { model = "m" })).Status);

        // No token at all is still 401 on a no-auth server — the grant is a role, not an open door.
        Assert.Equal(HttpStatusCode.Unauthorized, (await Call(dev, "GET", "/admin/settings")).Status);

        await dev.StopAsync();

        await using var authed = BuildAdminApp(AuthMode.Trusted);
        await authed.StartAsync();
        Assert.Equal(HttpStatusCode.Forbidden, (await Call(authed, "GET", "/admin/settings", Basic())).Status);
        await authed.StopAsync();
    }

    // ── shapes the console consumes ─────────────────────────────────────────────

    [Fact]
    public async Task Me_ReturnsTheConsolePrincipalShape()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();
        var (status, json) = await Call(app, "GET", "/admin/me", Curator());
        Assert.Equal(HttpStatusCode.OK, status);
        Assert.Equal("u-curator", json!["userId"]!.GetValue<string>());
        Assert.Equal("org-1", json["orgId"]!.GetValue<string>());
        Assert.Equal("curator", json["role"]!.GetValue<string>());
        await app.StopAsync();
    }

    [Fact]
    public async Task Conversations_AndMessages_CarryTheirEnvelopes()
    {
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", "Ada", "ada@example.com");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "how do I deploy?");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Outbound, "run the pipeline");

        await using var app = BuildAdminApp(AuthMode.Trusted, store: store);
        await app.StartAsync();
        var token = Token("u-ada", "org-1", "basic", "ada@example.com");

        var list = await Call(app, "GET", "/admin/conversations", token);
        Assert.Equal(HttpStatusCode.OK, list.Status);
        var conversations = list.Json!["conversations"]!.AsArray();
        Assert.Single(conversations);
        Assert.Equal(session.ConversationId, conversations[0]!["id"]!.GetValue<string>());
        Assert.Equal("how do I deploy?", conversations[0]!["name"]!.GetValue<string>());
        Assert.Equal("web", conversations[0]!["platform"]!.GetValue<string>());
        // The envelope carries the paging key even when there is no next page.
        Assert.True(list.Json.AsObject().ContainsKey("nextCursor"));
        Assert.Null(list.Json["nextCursor"]);

        var messages = await Call(app, "GET", $"/admin/conversations/{session.ConversationId}/messages", token);
        Assert.Equal(HttpStatusCode.OK, messages.Status);
        Assert.Equal(session.ConversationId, messages.Json!["conversationId"]!.GetValue<string>());
        var rows = messages.Json["messages"]!.AsArray();
        Assert.Equal(2, rows.Count);
        Assert.Equal("inbound", rows[0]!["direction"]!.GetValue<string>());
        Assert.Equal("how do I deploy?", rows[0]!["content"]!["text"]!.GetValue<string>());
        Assert.Equal("text", rows[0]!["content"]!["items"]!.AsArray()[0]!["type"]!.GetValue<string>());
        Assert.Equal("outbound", rows[1]!["direction"]!.GetValue<string>());

        await app.StopAsync();
    }

    [Fact]
    public async Task DocumentSets_AnswerWithAnEmptyList()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();
        var (status, json) = await Call(app, "GET", "/admin/document-sets", Curator());
        Assert.Equal(HttpStatusCode.OK, status);
        Assert.Empty(json!["documentSets"]!.AsArray());
        await app.StopAsync();
    }

    // ── connector CRUD ──────────────────────────────────────────────────────────

    [Fact]
    public async Task Connectors_RoundTrip_Create_List_Get_Update_Index_Delete()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();

        var created = await Call(app, "POST", "/admin/connectors", Admin(),
            new { name = "docs", kind = "web", config = new { url = "https://x" }, enabled = true });
        Assert.Equal(HttpStatusCode.OK, created.Status);
        var connector = created.Json!["connector"]!.AsObject();
        var id = connector["id"]!.GetValue<string>();
        Assert.NotEmpty(id);
        Assert.Equal("docs", connector["name"]!.GetValue<string>());
        Assert.Equal("web", connector["kind"]!.GetValue<string>());
        Assert.True(connector["enabled"]!.GetValue<bool>());
        Assert.Equal("https://x", connector["config"]!["url"]!.GetValue<string>());
        Assert.NotNull(connector["createdAt"]);
        // The internal owner key must never reach the wire.
        Assert.False(connector.ContainsKey("orgId"));

        var list = await Call(app, "GET", "/admin/connectors", Curator());
        Assert.Single(list.Json!["connectors"]!.AsArray());

        var got = await Call(app, "GET", $"/admin/connectors/{id}", Curator());
        Assert.Equal(id, got.Json!["connector"]!["id"]!.GetValue<string>());

        var updated = await Call(app, "PUT", $"/admin/connectors/{id}", Admin(),
            new { name = "docs2", kind = "web", config = new { }, enabled = false });
        Assert.Equal("docs2", updated.Json!["connector"]!["name"]!.GetValue<string>());
        Assert.False(updated.Json["connector"]!["enabled"]!.GetValue<bool>());

        var indexed = await Call(app, "POST", $"/admin/connectors/{id}/index", Curator(), new { });
        Assert.Equal(HttpStatusCode.OK, indexed.Status);
        Assert.Equal("docs2", indexed.Json!["run"]!["connectorName"]!.GetValue<string>());
        Assert.False(indexed.Json["run"]!.AsObject().ContainsKey("orgId"));

        var runs = await Call(app, "GET", "/admin/indexing/runs", Curator());
        Assert.Single(runs.Json!["runs"]!.AsArray());

        Assert.Equal(HttpStatusCode.NoContent, (await Call(app, "DELETE", $"/admin/connectors/{id}", Admin())).Status);
        Assert.Equal(HttpStatusCode.NotFound, (await Call(app, "GET", $"/admin/connectors/{id}", Curator())).Status);

        await app.StopAsync();
    }

    /// <summary>
    /// Org scoping lives in the handlers: a cross-org id must 404 identically to an unknown one, so
    /// the API is never an existence oracle for another org's rows.
    /// </summary>
    [Fact]
    public async Task Connectors_AreOrgIsolated_WithNoExistenceOracle()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();

        var created = await Call(app, "POST", "/admin/connectors", Admin(),
            new { name = "mine", kind = "web", config = new { }, enabled = true });
        var id = created.Json!["connector"]!["id"]!.GetValue<string>();

        var foreignAdmin = Token("u-other", "org-2", "admin");
        var known = await Call(app, "GET", $"/admin/connectors/{id}", foreignAdmin);
        var unknown = await Call(app, "GET", "/admin/connectors/does-not-exist", foreignAdmin);
        Assert.Equal(HttpStatusCode.NotFound, known.Status);
        Assert.Equal(HttpStatusCode.NotFound, unknown.Status);
        Assert.Equal(unknown.Json!.ToJsonString(), known.Json!.ToJsonString());

        Assert.Equal(HttpStatusCode.NotFound, (await Call(app, "PUT", $"/admin/connectors/{id}", foreignAdmin,
            new { name = "stolen", kind = "web", config = new { }, enabled = true })).Status);
        Assert.Equal(HttpStatusCode.NotFound, (await Call(app, "DELETE", $"/admin/connectors/{id}", foreignAdmin)).Status);
        Assert.Equal(HttpStatusCode.NotFound, (await Call(app, "POST", $"/admin/connectors/{id}/index", foreignAdmin, new { })).Status);

        Assert.Empty((await Call(app, "GET", "/admin/connectors", foreignAdmin)).Json!["connectors"]!.AsArray());
        // The owner still sees it — the isolation is scoping, not deletion.
        Assert.Single((await Call(app, "GET", "/admin/connectors", Curator())).Json!["connectors"]!.AsArray());

        await app.StopAsync();
    }

    [Fact]
    public async Task Connectors_ValidateTheWriteBody()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();
        var (status, json) = await Call(app, "POST", "/admin/connectors", Admin(), new { kind = "web" });
        Assert.Equal(HttpStatusCode.BadRequest, status);
        Assert.Equal("INVALID_BODY", json!["error"]!["code"]!.GetValue<string>());
        await app.StopAsync();
    }

    [Fact]
    public async Task IndexingRuns_AreOrgScoped()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();

        var created = await Call(app, "POST", "/admin/connectors", Admin(),
            new { name = "docs", kind = "web", config = new { }, enabled = true });
        var id = created.Json!["connector"]!["id"]!.GetValue<string>();
        await Call(app, "POST", $"/admin/connectors/{id}/index", Curator(), new { });

        Assert.Single((await Call(app, "GET", "/admin/indexing/runs", Curator())).Json!["runs"]!.AsArray());
        Assert.Empty((await Call(app, "GET", "/admin/indexing/runs", Token("u-other", "org-2", "admin"))).Json!["runs"]!.AsArray());

        await app.StopAsync();
    }

    // ── settings ────────────────────────────────────────────────────────────────

    /// <summary>A settings read with nothing stored returns DEFAULTS, never a 404.</summary>
    [Fact]
    public async Task Settings_ReadDefaultsOnAMiss_ThenRoundTripAWrite()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();

        var initial = await Call(app, "GET", "/admin/settings", Curator());
        Assert.Equal(HttpStatusCode.OK, initial.Status);
        Assert.Equal("org-1", initial.Json!["settings"]!["orgId"]!.GetValue<string>());
        Assert.NotEmpty(initial.Json["settings"]!["model"]!.GetValue<string>());
        Assert.Empty(initial.Json["settings"]!["defaultTools"]!.AsArray());

        var put = await Call(app, "PUT", "/admin/settings", Admin(),
            new { model = "claude-sonnet-4-5", systemPrompt = "be nice", defaultTools = new[] { "search" } });
        Assert.Equal(HttpStatusCode.OK, put.Status);
        Assert.Equal("claude-sonnet-4-5", put.Json!["settings"]!["model"]!.GetValue<string>());
        Assert.Equal("be nice", put.Json["settings"]!["systemPrompt"]!.GetValue<string>());

        var reread = await Call(app, "GET", "/admin/settings", Curator());
        Assert.Equal("claude-sonnet-4-5", reread.Json!["settings"]!["model"]!.GetValue<string>());
        Assert.Equal("search", reread.Json["settings"]!["defaultTools"]!.AsArray()[0]!.GetValue<string>());

        // Another org still reads its own defaults — settings are per-org, like every other row.
        var foreign = await Call(app, "GET", "/admin/settings", Token("u-other", "org-2", "admin"));
        Assert.Equal("org-2", foreign.Json!["settings"]!["orgId"]!.GetValue<string>());
        Assert.NotEqual("claude-sonnet-4-5", foreign.Json["settings"]!["model"]!.GetValue<string>());

        await app.StopAsync();
    }

    [Fact]
    public async Task Settings_RejectAWriteWithNoModel()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted);
        await app.StartAsync();
        var (status, json) = await Call(app, "PUT", "/admin/settings", Admin(), new { systemPrompt = "x" });
        Assert.Equal(HttpStatusCode.BadRequest, status);
        Assert.Equal("INVALID_BODY", json!["error"]!["code"]!.GetValue<string>());
        await app.StopAsync();
    }

    // ── model costs ─────────────────────────────────────────────────────────────

    /// <summary>
    /// One sequential test on purpose: the model-cost cache is process-wide, so the ungated read, the
    /// degrade-to-empty and the cache-only-success properties can only be asserted in a known order.
    /// The order is what proves the interesting part — a FAILED fetch must not be cached, or every
    /// cost badge stays missing until a restart even after the gateway recovers.
    /// </summary>
    [Fact]
    public async Task ModelCosts_IsUngated_DegradesToEmpty_AndCachesOnlySuccess()
    {
        const string urlVar = "SMOOAI_GATEWAY_URL";
        const string keyVar = "SMOOAI_GATEWAY_KEY";
        var originalUrl = Environment.GetEnvironmentVariable(urlVar);
        var originalKey = Environment.GetEnvironmentVariable(keyVar);

        try
        {
            // (a) An unreachable gateway degrades to `{}` at status 200, with NO token — the route is
            //     ungated because pricing is not org-sensitive and badges must render tokenless.
            Environment.SetEnvironmentVariable(urlVar, "http://127.0.0.1:1/v1");
            Environment.SetEnvironmentVariable(keyVar, null);

            await using var app = BuildAdminApp(AuthMode.Trusted);
            await app.StartAsync();

            var degraded = await app.GetTestServer().CreateClient().GetAsync("/admin/model-costs");
            Assert.Equal(HttpStatusCode.OK, degraded.StatusCode);
            Assert.Equal("{}", await degraded.Content.ReadAsStringAsync());

            // (b) Point at a live stub. If the failure above had been cached we would still see `{}`.
            await using var gateway = await StartStubGatewayAsync();
            Environment.SetEnvironmentVariable(urlVar, $"{gateway.BaseUrl}/v1");

            var mapped = await Call(app, "GET", "/admin/model-costs");
            Assert.Equal(HttpStatusCode.OK, mapped.Status);
            var opus = mapped.Json!["claude-opus-4-8"]!;
            Assert.Equal("frontier", opus["tier"]!.GetValue<string>());
            Assert.Equal(0.000075, opus["outputCostPerToken"]!.GetValue<double>(), precision: 12);
            // Omitted fields stay null rather than defaulting to a wrong number.
            Assert.Null(mapped.Json["cheap-model"]!["maxOutputTokens"]);

            // (c) Stop the stub and point somewhere dead again: a SUCCESS is cached for the process,
            //     so the same payload still comes back without another fetch.
            await gateway.App.StopAsync();
            Environment.SetEnvironmentVariable(urlVar, "http://127.0.0.1:1/v1");

            var cached = await Call(app, "GET", "/admin/model-costs");
            Assert.Equal(HttpStatusCode.OK, cached.Status);
            Assert.Equal("frontier", cached.Json!["claude-opus-4-8"]!["tier"]!.GetValue<string>());

            await app.StopAsync();
        }
        finally
        {
            Environment.SetEnvironmentVariable(urlVar, originalUrl);
            Environment.SetEnvironmentVariable(keyVar, originalKey);
        }
    }

    /// <summary>A real Kestrel stub standing in for the LiteLLM gateway's <c>/v1/model/info</c>.</summary>
    private sealed record StubGateway(WebApplication App, string BaseUrl) : IAsyncDisposable
    {
        public async ValueTask DisposeAsync() => await App.DisposeAsync();
    }

    private static async Task<StubGateway> StartStubGatewayAsync()
    {
        var builder = WebApplication.CreateBuilder();
        // A real port, not TestServer: the admin route fetches over a real HttpClient.
        builder.WebHost.UseUrls("http://127.0.0.1:0");
        var app = builder.Build();
        app.MapGet("/v1/model/info", () => Results.Text(
            """
            {
              "data": [
                {
                  "model_name": "claude-opus-4-8",
                  "model_info": {
                    "input_cost_per_token": 0.000015,
                    "output_cost_per_token": 0.000075,
                    "model_tier": "frontier",
                    "use_cases": ["reasoning"],
                    "max_output_tokens": 65536
                  }
                },
                { "model_name": "cheap-model", "model_info": { "input_cost_per_token": 0.0000008 } }
              ]
            }
            """,
            "application/json"));
        await app.StartAsync();
        var baseUrl = app.Urls.First();
        return new StubGateway(app, baseUrl);
    }

    // ── realtime publish ────────────────────────────────────────────────────────

    [Fact]
    public async Task Publish_DeliversToAnAttachedConnection()
    {
        var backplane = new InMemoryBackplane();
        JsonObject? received = null;
        backplane.Attach("conn-1", ev => received = ev);

        await using var app = BuildAdminApp(AuthMode.Trusted, backplane: backplane);
        await app.StartAsync();

        var (status, json) = await Call(app, "POST", "/admin/publish", Admin(),
            new { target = new { type = "connection", id = "conn-1" }, @event = new { kind = "job.done" } });

        Assert.Equal(HttpStatusCode.OK, status);
        Assert.Equal(1, json!["delivered"]!.GetValue<int>());
        Assert.NotNull(received);
        Assert.Equal("job.done", received!["kind"]!.GetValue<string>());

        await app.StopAsync();
    }

    /// <summary>
    /// A truthful zero: the target TYPE is routable here, the connection just is not attached. That is
    /// a real <c>delivered: 0</c>, unlike the unroutable target types below.
    /// </summary>
    [Fact]
    public async Task Publish_ReportsZeroForAnUnattachedConnection()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted, backplane: new InMemoryBackplane());
        await app.StartAsync();

        var (status, json) = await Call(app, "POST", "/admin/publish", Admin(),
            new { target = new { type = "connection", id = "nobody" }, @event = new { } });

        Assert.Equal(HttpStatusCode.OK, status);
        Assert.Equal(0, json!["delivered"]!.GetValue<int>());

        await app.StopAsync();
    }

    /// <summary>
    /// The whole point of the 501: session/user/org/agent are NOT routable by a connection-id registry.
    /// Answering <c>{"delivered": 0}</c> would read as "accepted, reached nobody" for an event that was
    /// never routable — so the response must carry no <c>delivered</c> field at all.
    /// </summary>
    [Theory]
    [InlineData("session")]
    [InlineData("user")]
    [InlineData("org")]
    [InlineData("agent")]
    public async Task Publish_RefusesTargetsTheBackplaneCannotRoute(string kind)
    {
        await using var app = BuildAdminApp(AuthMode.Trusted, backplane: new InMemoryBackplane());
        await app.StartAsync();

        var (status, json) = await Call(app, "POST", "/admin/publish", Admin(),
            new { target = new { type = kind, id = "x" }, @event = new { } });

        Assert.Equal(HttpStatusCode.NotImplemented, status);
        Assert.Equal("UNSUPPORTED_TARGET", json!["error"]!["code"]!.GetValue<string>());
        Assert.False(json.AsObject().ContainsKey("delivered"));

        await app.StopAsync();
    }

    [Fact]
    public async Task Publish_ValidatesTheBody()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted, backplane: new InMemoryBackplane());
        await app.StartAsync();

        // Missing target.id.
        Assert.Equal(HttpStatusCode.BadRequest, (await Call(app, "POST", "/admin/publish", Admin(),
            new { target = new { type = "connection" }, @event = new { } })).Status);

        // Unknown target type.
        Assert.Equal(HttpStatusCode.BadRequest, (await Call(app, "POST", "/admin/publish", Admin(),
            new { target = new { type = "wat", id = "x" }, @event = new { } })).Status);

        // No target at all.
        Assert.Equal(HttpStatusCode.BadRequest, (await Call(app, "POST", "/admin/publish", Admin(),
            new { @event = new { } })).Status);

        await app.StopAsync();
    }

    [Fact]
    public async Task Publish_IsAdminGated()
    {
        await using var app = BuildAdminApp(AuthMode.Trusted, backplane: new InMemoryBackplane());
        await app.StartAsync();

        var body = new { target = new { type = "connection", id = "x" }, @event = new { } };
        Assert.Equal(HttpStatusCode.Forbidden, (await Call(app, "POST", "/admin/publish", Curator(), body)).Status);
        Assert.Equal(HttpStatusCode.Unauthorized, (await Call(app, "POST", "/admin/publish", token: null, body: body)).Status);

        await app.StopAsync();
    }

    // ── this host's own extra route ─────────────────────────────────────────────

    [Fact]
    public async Task Reindex_FailsClosed_WithoutToken_AndRunsIngestion_WithToken()
    {
        var ingestion = new RepoIngestionService(
            new[] { new RepoSpec("acme", "docs", "main") },
            new AclKnowledgeStore(),
            _ => new MockConnector(new SourceDocument("d1", "runbook.md", "Deploy steps live here.")));

        await using var app = BuildAdminApp(AuthMode.Trusted, ingestion);
        await app.StartAsync();

        Assert.Equal(HttpStatusCode.Unauthorized, (await Call(app, "POST", "/admin/reindex")).Status);

        var (status, json) = await Call(app, "POST", "/admin/reindex", Admin());
        Assert.Equal(HttpStatusCode.OK, status);
        Assert.Equal("acme/docs", json!["results"]!.AsArray()[0]!["repo"]!.GetValue<string>());

        await app.StopAsync();
    }
}
