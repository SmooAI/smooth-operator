using System.Text.Json.Nodes;
using System.Text.Json.Serialization;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Routing;
using Microsoft.Extensions.DependencyInjection;

namespace SmooAI.SmoothOperator.Server.AspNetCore;

/// <summary>
/// The <c>/admin/*</c> management API — what the console (<c>console/</c>) drives.
/// <para>
/// Wire contract is the Rust server's <c>rust/smooth-operator-server/src/admin.rs</c>, matched
/// route-for-route by the Go (<c>go/server/admin.go</c>), TypeScript (<c>typescript/server/src/admin.ts</c>)
/// and Python (<c>python/.../admin.py</c>) servers: same paths, same <b>camelCase</b> JSON, the same
/// <c>{"error":{"code","message"}}</c> envelope, and the same role gate (Bearer token → verify → rank
/// check; 401 missing/invalid, 403 insufficient). Rank: basic=0, curator=1, admin=2.
/// </para>
/// <para>
/// Shapes are built against <c>console/lib/types.ts</c>, not Rust's field names: Rust's structs read
/// snake_case in source but carry <c>#[serde(rename_all = "camelCase")]</c>, so copying the field
/// names would produce a server that passes its own tests and renders nothing.
/// </para>
/// <para>
/// <c>POST /admin/reindex</c> is this host's own extra route (re-ingest the configured GitHub repos
/// without a restart); it has no sibling in the other four servers.
/// </para>
/// </summary>
public static class SmoothOperatorAdminExtensions
{
    /// <summary>Role ranks, mirroring Rust's <c>role_rank</c>.</summary>
    private const int RoleBasic = 0;
    private const int RoleCurator = 1;
    private const int RoleAdmin = 2;

    public static IEndpointRouteBuilder MapSmoothOperatorAdmin(this IEndpointRouteBuilder endpoints, string prefix = "/admin")
    {
        // ponytail: the admin state is per-mapping and in-memory, exactly as in the Go/TS/Python
        // servers. Resolved once here and captured by the handler closures, so it persists across
        // requests. A registered instance lets a host pre-seed or share that state — it is NOT a
        // storage swap point (see AdminStores); durable storage means changing that class.
        var stores = endpoints.ServiceProvider.GetService<AdminStores>() ?? new AdminStores();

        // The connection registry POST /admin/publish delivers through. AddSmoothOperatorServer
        // registers the shared in-memory default; the fallback here only covers an admin-only host with
        // no WebSocket endpoint, where there are no sinks to reach anyway.
        var backplane = endpoints.ServiceProvider.GetService<IBackplane>() ?? new InMemoryBackplane();

        // Ungated, exactly as in Rust: the console probes health before it has a token.
        endpoints.MapGet($"{prefix}/health", () => Results.Ok(new { status = "ok" }));

        // Also UNGATED, exactly as in Rust: gateway pricing is not org-sensitive, and the console's
        // cost badges must render on a tokenless local connection.
        //
        // Written with ToJsonString rather than Results.Ok: a top-level JsonObject handed to the
        // minimal-API JSON writer serializes to an EMPTY body (it round-trips fine as a property, e.g.
        // a connector's `config`, which is why nothing else here hits it). The map is already a JSON
        // document, so re-serializing it was pointless work anyway.
        endpoints.MapGet($"{prefix}/model-costs", async (HttpContext ctx) =>
        {
            var costs = await ModelCostsAsync(FetchModelCostsAsync, ctx.RequestAborted);
            return Results.Content(costs.ToJsonString(), "application/json");
        });

        endpoints.MapGet($"{prefix}/me", (HttpContext ctx) =>
        {
            var (gate, deny) = Authorize(ctx, RoleBasic);
            if (deny is not null) return deny;
            var p = gate!.Principal;
            return Results.Ok(new { userId = p.Sub, orgId = p.Org, role = RankName(RoleRank(p.Role)) });
        });

        endpoints.MapGet($"{prefix}/conversations", async (HttpContext ctx) =>
        {
            var (gate, deny) = Authorize(ctx, RoleBasic);
            if (deny is not null) return deny;

            var limit = int.TryParse(ctx.Request.Query["limit"], out var l) && l > 0 ? l : 50;
            var cursor = int.TryParse(ctx.Request.Query["cursor"], out var c) && c >= 0 ? c : 0;

            var store = ctx.RequestServices.GetService<ISessionStore>();
            var summaries = store is null
                ? Array.Empty<ConversationSummary>()
                : (await store.ListConversationsAsync(gate!.Access.ConversationScope, ctx.RequestAborted)).ToArray();
            Array.Sort(summaries, (a, b) => b.UpdatedAt.CompareTo(a.UpdatedAt));

            var page = summaries.Skip(cursor).Take(limit).ToArray();
            var end = Math.Min(cursor, summaries.Length) + page.Length;
            return Results.Ok(new
            {
                conversations = page.Select(s => new
                {
                    id = s.ConversationId,
                    name = string.IsNullOrEmpty(s.FirstInboundText) ? "Conversation" : s.FirstInboundText,
                    platform = "web",
                    createdAt = s.UpdatedAt,
                    updatedAt = s.UpdatedAt,
                }),
                nextCursor = end < summaries.Length ? end : (int?)null,
            });
        });

        endpoints.MapGet($"{prefix}/conversations/{{id}}/messages", async (HttpContext ctx, string id) =>
        {
            var (_, deny) = Authorize(ctx, RoleBasic);
            if (deny is not null) return deny;

            var store = ctx.RequestServices.GetService<ISessionStore>();
            var stored = store is null
                ? Array.Empty<StoredMessage>()
                : (await store.ListMessagesAsync(id, 500, ctx.RequestAborted)).ToArray();

            return Results.Ok(new
            {
                conversationId = id,
                messages = stored.Select(m => new
                {
                    id = m.Id,
                    conversationId = m.ConversationId,
                    direction = m.Direction == MessageDirection.Inbound ? "inbound" : "outbound",
                    content = new { items = new[] { new { type = "text", text = m.Text } }, text = m.Text },
                    createdAt = m.CreatedAt,
                }),
                nextCursor = (string?)null,
            });
        });

        endpoints.MapGet($"{prefix}/indexing/runs", (HttpContext ctx) =>
        {
            var (gate, deny) = Authorize(ctx, RoleCurator);
            if (deny is not null) return deny;
            lock (stores.Gate)
            {
                return Results.Ok(new { runs = stores.Runs.Where(r => r.OrgId == gate!.Principal.Org).ToArray() });
            }
        });

        endpoints.MapGet($"{prefix}/document-sets", (HttpContext ctx) =>
        {
            var (_, deny) = Authorize(ctx, RoleCurator);
            if (deny is not null) return deny;
            // ponytail: no per-document-set index on this server yet, so there are no sets to count.
            // An empty list is the honest answer and renders fine; wire it to the knowledge base when
            // one tracks set membership.
            return Results.Ok(new { documentSets = Array.Empty<object>() });
        });

        endpoints.MapGet($"{prefix}/connectors", (HttpContext ctx) =>
        {
            var (gate, deny) = Authorize(ctx, RoleCurator);
            if (deny is not null) return deny;
            lock (stores.Gate)
            {
                var rows = stores.Connectors.Values
                    .Where(c => c.OrgId == gate!.Principal.Org)
                    .OrderBy(c => c.Name, StringComparer.Ordinal)
                    .ToArray();
                return Results.Ok(new { connectors = rows });
            }
        });

        endpoints.MapPost($"{prefix}/connectors", async (HttpContext ctx) =>
        {
            var (gate, deny) = Authorize(ctx, RoleAdmin);
            if (deny is not null) return deny;

            var (body, bodyError) = await ReadJsonBodyAsync(ctx);
            if (bodyError is not null) return bodyError;
            var (write, writeError) = ValidateConnector(body);
            if (writeError is not null) return writeError;

            var now = DateTimeOffset.UtcNow;
            var row = new ConnectorRow
            {
                Id = Guid.NewGuid().ToString(),
                Name = write!.Name,
                Kind = write.Kind,
                Config = write.Config,
                Enabled = write.Enabled,
                CreatedAt = now,
                UpdatedAt = now,
                OrgId = gate!.Principal.Org,
            };
            lock (stores.Gate)
            {
                stores.Connectors[row.Id] = row;
            }
            return Results.Ok(new { connector = row });
        });

        endpoints.MapGet($"{prefix}/connectors/{{id}}", (HttpContext ctx, string id) =>
        {
            var (gate, deny) = Authorize(ctx, RoleCurator);
            if (deny is not null) return deny;
            lock (stores.Gate)
            {
                var row = OwnedConnector(stores, id, gate!.Principal.Org);
                return row is null ? NotFoundConnector() : Results.Ok(new { connector = row });
            }
        });

        endpoints.MapPut($"{prefix}/connectors/{{id}}", async (HttpContext ctx, string id) =>
        {
            var (gate, deny) = Authorize(ctx, RoleAdmin);
            if (deny is not null) return deny;

            var (body, bodyError) = await ReadJsonBodyAsync(ctx);
            if (bodyError is not null) return bodyError;
            var (write, writeError) = ValidateConnector(body);
            if (writeError is not null) return writeError;

            lock (stores.Gate)
            {
                var row = OwnedConnector(stores, id, gate!.Principal.Org);
                if (row is null) return NotFoundConnector();
                // Replace rather than mutate: rows are handed to the serializer, which runs AFTER
                // this lock releases, so an in-place edit could be torn by a concurrent write.
                var updated = new ConnectorRow
                {
                    Id = row.Id,
                    Name = write!.Name,
                    Kind = write.Kind,
                    Config = write.Config,
                    Enabled = write.Enabled,
                    CreatedAt = row.CreatedAt,
                    UpdatedAt = DateTimeOffset.UtcNow,
                    OrgId = row.OrgId,
                };
                stores.Connectors[id] = updated;
                return Results.Ok(new { connector = updated });
            }
        });

        endpoints.MapDelete($"{prefix}/connectors/{{id}}", (HttpContext ctx, string id) =>
        {
            var (gate, deny) = Authorize(ctx, RoleAdmin);
            if (deny is not null) return deny;
            lock (stores.Gate)
            {
                if (OwnedConnector(stores, id, gate!.Principal.Org) is null) return NotFoundConnector();
                stores.Connectors.Remove(id);
                return Results.NoContent();
            }
        });

        endpoints.MapPost($"{prefix}/connectors/{{id}}/index", (HttpContext ctx, string id) =>
        {
            var (gate, deny) = Authorize(ctx, RoleCurator);
            if (deny is not null) return deny;
            lock (stores.Gate)
            {
                var row = OwnedConnector(stores, id, gate!.Principal.Org);
                if (row is null) return NotFoundConnector();

                // ponytail: no per-connector ingestion pipeline on this server yet (the repo ingester
                // below is driven by env, not by these rows), so the run is recorded as succeeded with
                // zero documents rather than faked with invented counts.
                var now = DateTimeOffset.UtcNow;
                var run = new IndexingRunRow
                {
                    Id = Guid.NewGuid().ToString(),
                    ConnectorName = row.Name,
                    Status = "succeeded",
                    StartedAt = now,
                    FinishedAt = now,
                    OrgId = gate.Principal.Org,
                };
                stores.Runs.Add(run);
                return Results.Ok(new { run });
            }
        });

        endpoints.MapGet($"{prefix}/settings", (HttpContext ctx) =>
        {
            var (gate, deny) = Authorize(ctx, RoleCurator);
            if (deny is not null) return deny;
            lock (stores.Gate)
            {
                // Defaults on a miss, never a 404 — the console renders the settings form either way.
                var settings = stores.Settings.TryGetValue(gate!.Principal.Org, out var found)
                    ? found
                    : DefaultSettings(gate.Principal.Org);
                return Results.Ok(new { settings });
            }
        });

        endpoints.MapPut($"{prefix}/settings", async (HttpContext ctx) =>
        {
            var (gate, deny) = Authorize(ctx, RoleAdmin);
            if (deny is not null) return deny;

            var (body, bodyError) = await ReadJsonBodyAsync(ctx);
            if (bodyError is not null) return bodyError;

            var model = Str(body, "model");
            if (string.IsNullOrWhiteSpace(model))
            {
                return AdminError(StatusCodes.Status400BadRequest, "INVALID_BODY", "model is required");
            }

            var settings = new AgentSettingsRow
            {
                OrgId = gate!.Principal.Org,
                Model = model!,
                SystemPrompt = Str(body, "systemPrompt") ?? string.Empty,
                DefaultTools = StrArray(body, "defaultTools"),
                UpdatedAt = DateTimeOffset.UtcNow,
            };
            lock (stores.Gate)
            {
                stores.Settings[gate.Principal.Org] = settings;
            }
            return Results.Ok(new { settings });
        });

        // ── Realtime publish ────────────────────────────────────────────────────────
        // Push an event to a backplane target over the connection fleet — the plug point for non-AI
        // publishers (job status, ingestion progress, notifications) that need to reach a connected
        // client without going through an agent turn. Admin-gated.
        //
        // This server's backplane is a connectionId→sink registry, so only `connection` targets can be
        // routed. Rust additionally fans out to session/user/org/agent over a richer backplane; here
        // those are a hard 501 rather than a misleading `{"delivered": 0}` — a caller must never read
        // "accepted, reached nobody" as success for an event that was never routable in the first
        // place. When the fan-out lands, each target flips from a 501 to a real count.
        endpoints.MapPost($"{prefix}/publish", async (HttpContext ctx) =>
        {
            var (_, deny) = Authorize(ctx, RoleAdmin);
            if (deny is not null) return deny;

            var (body, bodyError) = await ReadJsonBodyAsync(ctx);
            if (bodyError is not null) return bodyError;

            var target = body?["target"] as JsonObject;
            var kind = (Str(target, "type") ?? string.Empty).Trim().ToLowerInvariant();
            var id = (Str(target, "id") ?? string.Empty).Trim();
            if (id.Length == 0)
            {
                return AdminError(StatusCodes.Status400BadRequest, "INVALID_BODY", "target.id is required");
            }

            switch (kind)
            {
                case "connection":
                    var payload = body?["event"] is JsonObject ev ? (JsonObject)ev.DeepClone() : new JsonObject();
                    return Results.Ok(new { delivered = backplane.Publish(id, payload) });

                case "session":
                case "user":
                case "org":
                case "agent":
                    return AdminError(StatusCodes.Status501NotImplemented, "UNSUPPORTED_TARGET",
                        $"this server's backplane routes by connection id only; \"{kind}\" targets are not deliverable here");

                default:
                    return AdminError(StatusCodes.Status400BadRequest, "INVALID_BODY",
                        $"unknown target type \"{kind}\" (want connection|session|user|org|agent)");
            }
        });

        // ── This host's own extra: re-ingest every configured repo without a restart. ──
        endpoints.MapPost($"{prefix}/reindex", async (HttpContext ctx) =>
        {
            var (_, deny) = Authorize(ctx, RoleAdmin);
            if (deny is not null) return deny;

            var service = ctx.RequestServices.GetService<RepoIngestionService>();
            if (service is null)
            {
                return Results.Ok(new { results = Array.Empty<object>() });
            }
            var results = await service.ReindexAllAsync(ctx.RequestAborted);
            return Results.Ok(new
            {
                results = results.Select(r => new { repo = r.Repo, documents = r.Documents, chunks = r.Chunks, ok = r.Ok, error = r.Error }),
            });
        });

        return endpoints;
    }

    // ── auth gate ───────────────────────────────────────────────────────────────

    /// <summary>The authenticated caller, plus the access context its data scoping is derived from.</summary>
    private sealed record AuthorizedCaller(AccessContext Access, Principal Principal);

    /// <summary>
    /// Authenticate the request and enforce a minimum role. Returns the caller, or the rejection to
    /// return. Fails CLOSED: no bearer token is 401 even on a no-auth server.
    /// </summary>
    private static (AuthorizedCaller? Caller, IResult? Deny) Authorize(HttpContext ctx, int min)
    {
        var token = BearerToken(ctx);
        if (token is null)
        {
            return (null, AdminError(StatusCodes.Status401Unauthorized, "UNAUTHENTICATED", "missing bearer token"));
        }

        // Same seam, same order as the WebSocket host. ponytail: a host that registered NO verifier
        // gets 401 rather than a defaulted NoAuthVerifier — defaulting would hand any token the
        // AUTH_MODE=none admin grant below, which is the opposite of failing closed.
        var verifier = ctx.RequestServices.GetService<IAuthVerifier>()
            ?? (IAuthVerifier?)ctx.RequestServices.GetService<TokenAccessResolver>();
        if (verifier is null)
        {
            return (null, AdminError(StatusCodes.Status401Unauthorized, "UNAUTHENTICATED", "no auth verifier configured"));
        }

        var access = verifier.Resolve(token);
        // An auth-enabled server that could not verify the token yields an anonymous context, which
        // must never satisfy an admin route.
        if (access.AuthEnabled && access.IsAnonymous)
        {
            return (null, AdminError(StatusCodes.Status401Unauthorized, "INVALID_TOKEN", "invalid bearer token"));
        }

        var principal = access.Principal;
        // AUTH_MODE=none (dev) grants Admin, exactly as Rust's NoAuthVerifier does — otherwise the
        // console 403-walls against a local server, which is as useless as the 404s this API exists to
        // fix. Only the explicit no-auth verifier takes this path; an auth-enabled server is unaffected.
        if (verifier.Mode == "none")
        {
            principal = principal with { Role = "admin" };
        }

        var rank = RoleRank(principal.Role);
        if (rank < min)
        {
            return (null, AdminError(StatusCodes.Status403Forbidden, "FORBIDDEN",
                $"requires role {RankName(min)}, principal has {RankName(rank)}"));
        }
        return (new AuthorizedCaller(access, principal), null);
    }

    /// <summary>The raw token from <c>Authorization: Bearer &lt;token&gt;</c>, or null.</summary>
    private static string? BearerToken(HttpContext ctx)
    {
        var value = ctx.Request.Headers.Authorization.FirstOrDefault();
        if (string.IsNullOrEmpty(value) || !value.StartsWith("Bearer ", StringComparison.OrdinalIgnoreCase))
        {
            return null;
        }
        var token = value["Bearer ".Length..].Trim();
        return token.Length == 0 ? null : token;
    }

    /// <summary>Unknown/empty roles are basic — fail closed on privilege, not open.</summary>
    private static int RoleRank(string? role) => role?.Trim().ToLowerInvariant() switch
    {
        "admin" => RoleAdmin,
        "curator" => RoleCurator,
        _ => RoleBasic,
    };

    private static string RankName(int rank) => rank switch
    {
        RoleAdmin => "admin",
        RoleCurator => "curator",
        _ => "basic",
    };

    // ── model costs ─────────────────────────────────────────────────────────────

    private static readonly object ModelCostsGate = new();

    /// <summary>
    /// The mapped <c>/model/info</c> payload for the process. Gateway pricing is stable, so one fetch
    /// per process is enough (matching Rust's <c>OnceCell</c>). Only a SUCCESS is ever stored here —
    /// caching a failure would pin an empty map for the whole process, so every cost badge would stay
    /// missing until a restart even after the gateway recovered.
    /// </summary>
    private static JsonObject? _modelCostsCache;

    /// <summary>Ten seconds, matching the Go and TS ports. Reused so a per-request client isn't minted.</summary>
    private static readonly HttpClient ModelCostsHttp = new() { Timeout = TimeSpan.FromSeconds(10) };

    /// <summary>
    /// The cached model-cost map, fetching once per process via <paramref name="fetch"/>. Any failure
    /// degrades to an empty object with status 200 — never a 500, since a missing badge beats a broken
    /// page — and is deliberately NOT cached, so the next request retries.
    /// </summary>
    private static async Task<JsonObject> ModelCostsAsync(Func<CancellationToken, Task<JsonObject>> fetch, CancellationToken cancellationToken)
    {
        lock (ModelCostsGate)
        {
            if (_modelCostsCache is not null)
            {
                return _modelCostsCache;
            }
        }

        JsonObject mapped;
        try
        {
            mapped = await fetch(cancellationToken).ConfigureAwait(false);
        }
        catch
        {
            return new JsonObject();
        }

        lock (ModelCostsGate)
        {
            // A lost race is harmless — both callers mapped the same stable pricing.
            return _modelCostsCache ??= mapped;
        }
    }

    /// <summary>
    /// GET the gateway's <c>/model/info</c> with the server's configured gateway credentials — the
    /// same ones the turns use — and map it. Reads the cross-engine <c>SMOOAI_*</c> names Go and TS
    /// read, with this host's <c>SMOOTH_*</c> aliases honored too so a deployment that set only those
    /// gets badges rather than a silently empty map.
    /// </summary>
    private static async Task<JsonObject> FetchModelCostsAsync(CancellationToken cancellationToken)
    {
        var baseUrl = ServerEnv.First(
            Environment.GetEnvironmentVariable("SMOOAI_GATEWAY_URL"),
            Environment.GetEnvironmentVariable("SMOOTH_GATEWAY_URL"),
            "https://llm.smoo.ai/v1").TrimEnd('/');
        var key = ServerEnv.First(
            Environment.GetEnvironmentVariable("SMOOAI_GATEWAY_KEY"),
            Environment.GetEnvironmentVariable("SMOOTH_GATEWAY_KEY"));

        using var request = new HttpRequestMessage(HttpMethod.Get, $"{baseUrl}/model/info");
        if (key.Length > 0)
        {
            request.Headers.Authorization = new System.Net.Http.Headers.AuthenticationHeaderValue("Bearer", key);
        }
        using var response = await ModelCostsHttp.SendAsync(request, cancellationToken).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();
        var body = await response.Content.ReadAsStringAsync(cancellationToken).ConfigureAwait(false);
        return ModelInfo.MapModelInfo(JsonNode.Parse(body));
    }

    // ── bodies + responses ──────────────────────────────────────────────────────

    private static IResult AdminError(int status, string code, string message) =>
        Results.Json(new { error = new { code, message } }, statusCode: status);

    private static IResult NotFoundConnector() =>
        AdminError(StatusCodes.Status404NotFound, "NOT_FOUND", "connector not found");

    private static async Task<(JsonObject? Body, IResult? Error)> ReadJsonBodyAsync(HttpContext ctx)
    {
        // No JSON body at all reads as `{}` — the per-field validation below produces the 400, with a
        // message naming the missing field rather than a blanket "malformed".
        if (!ctx.Request.HasJsonContentType())
        {
            return (null, null);
        }
        try
        {
            return (await ctx.Request.ReadFromJsonAsync<JsonObject>(ctx.RequestAborted), null);
        }
        catch (Exception ex) when (ex is System.Text.Json.JsonException or BadHttpRequestException)
        {
            return (null, AdminError(StatusCodes.Status400BadRequest, "INVALID_BODY", "malformed JSON body"));
        }
    }

    private static string? Str(JsonObject? body, string key) =>
        body?[key] is JsonValue v && v.TryGetValue<string>(out var s) ? s : null;

    private static bool Bool(JsonObject? body, string key) =>
        body?[key] is JsonValue v && v.TryGetValue<bool>(out var b) && b;

    private static string[] StrArray(JsonObject? body, string key) =>
        body?[key] is JsonArray a
            ? a.OfType<JsonValue>().Select(v => v.TryGetValue<string>(out var s) ? s : null).OfType<string>().ToArray()
            : Array.Empty<string>();

    private sealed record ConnectorWrite(string Name, string Kind, JsonObject Config, bool Enabled);

    /// <summary>Validate a connector write body, or return the 400.</summary>
    private static (ConnectorWrite? Write, IResult? Error) ValidateConnector(JsonObject? body)
    {
        var name = Str(body, "name");
        var kind = Str(body, "kind");
        if (string.IsNullOrWhiteSpace(name) || string.IsNullOrWhiteSpace(kind))
        {
            return (null, AdminError(StatusCodes.Status400BadRequest, "INVALID_BODY", "name and kind are required"));
        }
        // DeepClone: a JsonNode may only ever have one parent, so the caller's node cannot be stored.
        var config = body?["config"] is JsonObject c ? (JsonObject)c.DeepClone() : new JsonObject();
        return (new ConnectorWrite(name!, kind!, config, Bool(body, "enabled")), null);
    }

    /// <summary>Rust's "defaults when unset" settings read.</summary>
    private static AgentSettingsRow DefaultSettings(string orgId) => new()
    {
        OrgId = orgId,
        Model = "claude-haiku-4-5",
        SystemPrompt = string.Empty,
        DefaultTools = Array.Empty<string>(),
        UpdatedAt = DateTimeOffset.UtcNow,
    };

    private static ConnectorRow? OwnedConnector(AdminStores stores, string id, string orgId)
    {
        // A cross-org id is deliberately indistinguishable from an unknown one — no existence oracle.
        return stores.Connectors.TryGetValue(id, out var row) && row.OrgId == orgId ? row : null;
    }
}

/// <summary>
/// Org-scoped admin state. Every read and write filters by org, so one org can never see or mutate
/// another's rows. Register one in DI to override the per-mapping default (e.g. to seed fixtures).
/// <para>
/// ponytail: in-memory, and sealed with get-only collections — a registered instance can be
/// pre-seeded or shared, but it cannot be handed Postgres-backed collections. Durable storage means
/// changing THIS class, not registering a different one; the ceiling is deliberate, since one
/// implementation exists and an interface for a second nobody has scheduled would be speculative.
/// If durable admin storage lands (converged onto Rust's <c>ADMIN_SCHEMA</c>), keep the org filter
/// in the HANDLERS: moving it into the store is how a cross-org id stops 404-ing identically to an
/// unknown one, which is what makes this API not an existence oracle.
/// </para>
/// </summary>
public sealed class AdminStores
{
    /// <summary>Guards all three collections — the routes are served concurrently.</summary>
    public object Gate { get; } = new();

    public Dictionary<string, ConnectorRow> Connectors { get; } = new();

    public Dictionary<string, AgentSettingsRow> Settings { get; } = new();

    public List<IndexingRunRow> Runs { get; } = new();
}

/// <summary>
/// A persisted, org-scoped connector config — the <c>ConnectorConfig</c> in console/lib/types.ts.
/// Immutable: an update replaces the row, so a row already handed to the serializer can never be
/// torn by a concurrent write.
/// </summary>
public sealed class ConnectorRow
{
    public required string Id { get; init; }

    public required string Name { get; init; }

    public required string Kind { get; init; }

    public required JsonObject Config { get; init; }

    public required bool Enabled { get; init; }

    public required DateTimeOffset CreatedAt { get; init; }

    public required DateTimeOffset UpdatedAt { get; init; }

    /// <summary>
    /// The owning org. Internal — never serialized to the wire. Deliberately NOT <c>required</c>:
    /// System.Text.Json rejects a required property it is told to ignore, since it could never
    /// satisfy it on the way back in.
    /// </summary>
    [JsonIgnore]
    public string OrgId { get; init; } = string.Empty;
}

/// <summary>Per-org agent settings — the <c>AgentSettings</c> in console/lib/types.ts. <c>orgId</c> IS on the wire here.</summary>
public sealed class AgentSettingsRow
{
    public required string OrgId { get; init; }

    public required string Model { get; init; }

    public required string SystemPrompt { get; init; }

    public required IReadOnlyList<string> DefaultTools { get; init; }

    public required DateTimeOffset UpdatedAt { get; init; }
}

/// <summary>One indexing run — the <c>IndexingRun</c> in console/lib/types.ts.</summary>
public sealed class IndexingRunRow
{
    public required string Id { get; init; }

    public required string ConnectorName { get; init; }

    public required string Status { get; init; }

    public required DateTimeOffset StartedAt { get; init; }

    public DateTimeOffset? FinishedAt { get; init; }

    public int DocumentsSeen { get; init; }

    public int ChunksIndexed { get; init; }

    public int DocumentsSkipped { get; init; }

    public string? Error { get; init; }

    /// <summary>
    /// The owning org. Internal — never serialized to the wire. Deliberately NOT <c>required</c>:
    /// System.Text.Json rejects a required property it is told to ignore, since it could never
    /// satisfy it on the way back in.
    /// </summary>
    [JsonIgnore]
    public string OrgId { get; init; } = string.Empty;
}
