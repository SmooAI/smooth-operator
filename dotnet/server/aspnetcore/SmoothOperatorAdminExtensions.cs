using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Routing;
using Microsoft.Extensions.DependencyInjection;

namespace SmooAI.SmoothOperator.Server.AspNetCore;

/// <summary>
/// The auth-gated admin HTTP API, the C# analog of the Rust server's <c>/admin</c> router. A focused
/// subset for the current deploy: <c>/admin/health</c> (ungated), <c>/admin/me</c> (whoami),
/// <c>/admin/connectors</c> (the configured repos), and <c>/admin/reindex</c> (re-ingest without a
/// restart). Everything except health is gated by <see cref="TokenAccessResolver"/>: a request whose
/// <c>Authorization: Bearer …</c> (or <c>?token=</c>) doesn't resolve to a non-anonymous identity gets
/// 401 — fail-closed, mirroring the Rust <c>require_role</c> (and "auth unset ⇒ admin 401").
/// </summary>
public static class SmoothOperatorAdminExtensions
{
    public static IEndpointRouteBuilder MapSmoothOperatorAdmin(this IEndpointRouteBuilder endpoints, string prefix = "/admin")
    {
        // Ungated liveness — safe to expose to a load balancer.
        endpoints.MapGet($"{prefix}/health", () => Results.Ok(new { status = "ok" }));

        // Whoami — proves the auth chain over HTTP and lets a console show the resolved identity.
        endpoints.MapGet($"{prefix}/me", (HttpContext ctx) =>
        {
            var access = ResolveAccess(ctx);
            if (access.IsAnonymous)
            {
                return Results.Unauthorized();
            }
            var p = access.Principal;
            return Results.Ok(new { sub = p.Sub, org = p.Org, role = p.Role, groups = p.Groups });
        });

        // The configured repos (read-only) — what the server will index.
        endpoints.MapGet($"{prefix}/connectors", (HttpContext ctx) =>
        {
            if (ResolveAccess(ctx).IsAnonymous)
            {
                return Results.Unauthorized();
            }
            var service = ctx.RequestServices.GetService<RepoIngestionService>();
            var connectors = (service?.ConfiguredRepos ?? Array.Empty<RepoSpec>())
                .Select(r => new { id = r.Slug, owner = r.Owner, repo = r.Repo, gitRef = r.GitRef, aclGroup = r.AclGroup });
            return Results.Ok(new { connectors });
        });

        // Trigger a re-ingest of every configured repo without restarting the host.
        endpoints.MapPost($"{prefix}/reindex", async (HttpContext ctx) =>
        {
            if (ResolveAccess(ctx).IsAnonymous)
            {
                return Results.Unauthorized();
            }
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

    /// <summary>Resolve the request's identity from the <c>Authorization: Bearer …</c> header (or
    /// <c>?token=</c> fallback). Fails closed to anonymous when no resolver is registered.</summary>
    private static AccessContext ResolveAccess(HttpContext ctx)
    {
        var resolver = ctx.RequestServices.GetService<TokenAccessResolver>();
        if (resolver is null)
        {
            return AccessContext.Anonymous;
        }

        string? token = null;
        var header = ctx.Request.Headers.Authorization.FirstOrDefault();
        if (!string.IsNullOrEmpty(header))
        {
            token = header.StartsWith("Bearer ", StringComparison.OrdinalIgnoreCase) ? header["Bearer ".Length..].Trim() : header;
        }
        token ??= ctx.Request.Query["token"].FirstOrDefault();

        return resolver.Resolve(token);
    }
}
