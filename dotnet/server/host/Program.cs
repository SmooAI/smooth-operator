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

// ── Reranker: opt-in post-retrieval reorder (SMOOTH_AGENT_RERANK=gateway|lexical|off). Off by
//    default, so retrieval order is unchanged unless explicitly enabled. The gateway cross-encoder
//    is used only when a key is present; otherwise it falls back to the offline lexical reranker. ──
var rerankMode = RerankSelection.ParseMode(Get("SMOOTH_AGENT_RERANK"));
var reranker = RerankSelection.Build(
    rerankMode,
    hasGatewayKey: !string.IsNullOrEmpty(gatewayKey),
    Get("SMOOTH_RERANK_MODEL", RerankSelection.DefaultRerankModel),
    () => EmbeddingHttpClient(gatewayUrl, gatewayKey)); // same gateway base + auth as embeddings
if (reranker is not null)
{
    builder.Services.AddSingleton(reranker);
}

// ── Auth: jwt (verified) / trusted (proxied) / none ──
var authMode = Enum.TryParse<AuthMode>(Get("SMOOTH_AUTH_MODE", "none"), ignoreCase: true, out var mode) ? mode : AuthMode.None;
builder.Services.AddSingleton(new TokenAccessResolver(new AuthOptions
{
    Mode = authMode,
    Hs256Secret = Get("SMOOTH_JWT_HS256_SECRET"),
}));

// ── Repo ingestion service: parses SMOOTH_GITHUB_REPOS into RepoSpecs, ingests each into the
//    ACL-aware store stamped with its github:owner/repo group. Registered so it serves both the
//    startup ingest AND the POST /admin/reindex endpoint (re-index without a restart). ──
var repos = Get("SMOOTH_GITHUB_REPOS")
    .Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
    .Select(RepoSpec.Parse)
    .ToArray();
var githubToken = Get("SMOOTH_GITHUB_TOKEN");
builder.Services.AddSingleton(sp =>
{
    var http = new HttpClient();
    http.DefaultRequestHeaders.UserAgent.ParseAdd("smooth-operator-server");
    if (!string.IsNullOrEmpty(githubToken))
    {
        http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", githubToken);
    }
    return new RepoIngestionService(repos, knowledge, spec => new GitHubConnector(spec.Owner, spec.Repo, http, spec.GitRef));
});

// ── Write-confirmation HITL: SMOOTH_AGENT_CONFIRM_TOOLS (comma-separated tool-name substrings).
//    A turn that calls a matching tool parks and emits write_confirmation_required; the client
//    resumes it with confirm_tool_action. Unset (the default) ⇒ no tool ever requires confirmation
//    (behavior unchanged). Mirrors the Rust host's SMOOTH_AGENT_CONFIRM_TOOLS env var. ──
var confirmTools = Get("SMOOTH_AGENT_CONFIRM_TOOLS")
    .Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
if (confirmTools.Length > 0)
{
    builder.Services.AddSingleton(new ConfirmTools(confirmTools));
}

builder.Services.AddSmoothOperatorServer();

var app = builder.Build();

app.MapGet("/health", () => Results.Ok(new { status = "ok", model, auth = authMode.ToString().ToLowerInvariant() }));
app.MapSmoothOperatorWebSocket("/ws");
app.MapSmoothOperatorAdmin();

// ── Startup ingestion of configured repos (background — doesn't block readiness). The same
//    service backs POST /admin/reindex, so docs can be re-indexed later without a restart. ──
if (repos.Length > 0)
{
    var ingestion = app.Services.GetRequiredService<RepoIngestionService>();
    _ = ingestion.ReindexAllAsync();
}

app.Run();

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
