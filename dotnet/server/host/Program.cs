using System.ClientModel;
using System.Net.Http.Headers;
using Microsoft.Extensions.AI;
using OpenAI;
using SmooAI.SmoothOperator.Server;
using SmooAI.SmoothOperator.Server.AspNetCore;
using SmooAI.SmoothOperator.Server.Postgres;

// ─────────────────────────────────────────────────────────────────────────────
// A runnable smooth-operator server in C#. Wires the model, storage, auth, and
// GitHub ingestion from environment config, then serves the protocol over /ws.
// See README.md for the full env-var list.
// ─────────────────────────────────────────────────────────────────────────────

var builder = WebApplication.CreateBuilder(args);
var config = builder.Configuration; // already layered with environment variables

string Get(string key, string? fallback = null) =>
    config[key] is { Length: > 0 } value ? value : fallback ?? string.Empty;

// ── Model: an OpenAI-compatible gateway (the smooth gateway, Azure OpenAI, Ollama, …) ──
var gatewayUrl = Get("SMOOTH_GATEWAY_URL", "https://llm.smoo.ai/v1");
var gatewayKey = Get("SMOOTH_GATEWAY_KEY");
var model = Get("SMOOTH_MODEL", "claude-haiku-4-5");
builder.Services.AddSingleton<IChatClient>(_ =>
    new OpenAIClient(
            new ApiKeyCredential(string.IsNullOrEmpty(gatewayKey) ? "unset" : gatewayKey),
            new OpenAIClientOptions { Endpoint = new Uri(gatewayUrl) })
        .GetChatClient(model)
        .AsIChatClient());

// ── Storage: durable Postgres when configured, else in-memory (the default) ──
var databaseUrl = Get("SMOOTH_DATABASE_URL");
if (!string.IsNullOrEmpty(databaseUrl))
{
    builder.Services.AddSingleton<ISessionStore>(_ =>
        PostgresSessionStore.CreateAsync(ToNpgsqlConnectionString(databaseUrl)).GetAwaiter().GetResult());
}

// ── Knowledge: ACL-aware store (durable Postgres+pgvector when a DB is configured, else in-memory),
//    ingested at startup; ACL is enforced on the chat path. The durable store uses semantic
//    gateway embeddings when a key is present, else the deterministic fallback. ──
IEmbedder embedder = string.IsNullOrEmpty(gatewayKey)
    ? new DeterministicEmbedder()
    : new GatewayEmbedder(EmbeddingHttpClient(gatewayUrl, gatewayKey), Get("SMOOTH_EMBEDDING_MODEL", "text-embedding-3-small"), dimensions: 1536);
IAclKnowledge knowledge = string.IsNullOrEmpty(databaseUrl)
    ? new AclKnowledgeStore()
    : await PostgresAclKnowledgeStore.CreateAsync(ToNpgsqlConnectionString(databaseUrl), embedder);
builder.Services.AddSingleton<IAccessKnowledge>(knowledge);

// ── Auth: jwt (verified) / trusted (proxied) / none ──
var authMode = Enum.TryParse<AuthMode>(Get("SMOOTH_AUTH_MODE", "none"), ignoreCase: true, out var mode) ? mode : AuthMode.None;
builder.Services.AddSingleton(new TokenAccessResolver(new AuthOptions
{
    Mode = authMode,
    Hs256Secret = Get("SMOOTH_JWT_HS256_SECRET"),
}));

builder.Services.AddSmoothOperatorServer();

var app = builder.Build();

app.MapGet("/health", () => Results.Ok(new { status = "ok", model, auth = authMode.ToString().ToLowerInvariant() }));
app.MapSmoothOperatorWebSocket("/ws");

// ── Background ingestion of configured GitHub repos (doesn't block readiness) ──
var repos = Get("SMOOTH_GITHUB_REPOS").Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
if (repos.Length > 0)
{
    _ = IngestReposAsync(repos, Get("SMOOTH_GITHUB_TOKEN"), knowledge, app.Logger);
}

app.Run();

static async Task IngestReposAsync(string[] repos, string token, IAclKnowledge knowledge, ILogger logger)
{
    using var http = new HttpClient();
    http.DefaultRequestHeaders.UserAgent.ParseAdd("smooth-operator-server");
    if (!string.IsNullOrEmpty(token))
    {
        http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", token);
    }

    foreach (var spec in repos)
    {
        var (owner, repo, gitRef) = ParseRepo(spec);
        // Each repo's docs are entitled to the group `github:owner/repo` — a user's JWT/Okta groups
        // must include it (or the doc be public) to retrieve them.
        var aclGroup = $"github:{owner}/{repo}";
        var pipeline = new IngestPipeline(knowledge.WithAcl(DocumentAcl.ForGroups(aclGroup)));
        try
        {
            var result = await pipeline.IngestAsync(new GitHubConnector(owner, repo, http, gitRef));
            logger.LogInformation("Ingested {Repo}: {Docs} docs, {Chunks} chunks (acl {Acl})", spec, result.Documents, result.Chunks, aclGroup);
        }
        catch (Exception ex)
        {
            logger.LogError(ex, "Failed to ingest {Repo}", spec);
        }
    }
}

static (string Owner, string Repo, string Ref) ParseRepo(string spec)
{
    var atSplit = spec.Split('@', 2);
    var gitRef = atSplit.Length > 1 ? atSplit[1] : "main";
    var slashSplit = atSplit[0].Split('/', 2);
    return (slashSplit[0], slashSplit.Length > 1 ? slashSplit[1] : string.Empty, gitRef);
}

static HttpClient EmbeddingHttpClient(string gatewayUrl, string key)
{
    var baseUrl = gatewayUrl.EndsWith('/') ? gatewayUrl : gatewayUrl + "/";
    var http = new HttpClient { BaseAddress = new Uri(baseUrl) };
    http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", key);
    return http;
}

static string ToNpgsqlConnectionString(string url)
{
    if (!url.StartsWith("postgres://", StringComparison.OrdinalIgnoreCase) &&
        !url.StartsWith("postgresql://", StringComparison.OrdinalIgnoreCase))
    {
        return url; // already a key=value Npgsql connection string
    }

    var uri = new Uri(url);
    var userInfo = uri.UserInfo.Split(':', 2);
    var user = Uri.UnescapeDataString(userInfo[0]);
    var password = userInfo.Length > 1 ? Uri.UnescapeDataString(userInfo[1]) : string.Empty;
    var database = uri.AbsolutePath.TrimStart('/');
    var port = uri.Port > 0 ? uri.Port : 5432;
    return $"Host={uri.Host};Port={port};Username={user};Password={password};Database={database}";
}

/// <summary>Exposed so the host test project can boot the app via WebApplicationFactory.</summary>
public partial class Program
{
}
