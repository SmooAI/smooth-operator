using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>A repo to ingest, parsed from an <c>owner/repo[@ref]</c> spec.</summary>
public sealed record RepoSpec(string Owner, string Repo, string GitRef)
{
    /// <summary><c>owner/repo</c> — the display slug.</summary>
    public string Slug => $"{Owner}/{Repo}";

    /// <summary>The ACL group documents from this repo are entitled to (<c>github:owner/repo</c>).</summary>
    public string AclGroup => $"github:{Owner}/{Repo}";

    /// <summary>Parse an <c>owner/repo[@ref]</c> spec (ref defaults to <c>main</c>).</summary>
    public static RepoSpec Parse(string spec)
    {
        var atSplit = spec.Split('@', 2);
        var gitRef = atSplit.Length > 1 ? atSplit[1] : "main";
        var slashSplit = atSplit[0].Split('/', 2);
        return new RepoSpec(slashSplit[0], slashSplit.Length > 1 ? slashSplit[1] : string.Empty, gitRef);
    }
}

/// <summary>The outcome of ingesting one repo (or its error).</summary>
public sealed record RepoIngestResult(string Repo, int Documents, int Chunks, string? Error)
{
    public bool Ok => Error is null;
}

/// <summary>
/// Ingests the configured repos into the ACL-aware knowledge store, each stamped with its
/// <c>github:owner/repo</c> ACL group. Used at startup AND by the <c>POST /admin/reindex</c>
/// endpoint, so an operator can re-index after docs change without restarting the host. The
/// connector is built by an injected factory so tests can supply a <see cref="MockConnector"/>
/// (the real host uses a <see cref="GitHubConnector"/>). One repo's failure is captured in its
/// <see cref="RepoIngestResult"/> rather than aborting the whole run.
/// </summary>
public sealed class RepoIngestionService
{
    private readonly IReadOnlyList<RepoSpec> _repos;
    private readonly IAclKnowledge _knowledge;
    private readonly Func<RepoSpec, IConnector> _connectorFactory;

    public RepoIngestionService(IEnumerable<RepoSpec> repos, IAclKnowledge knowledge, Func<RepoSpec, IConnector> connectorFactory)
    {
        _repos = repos?.ToArray() ?? throw new ArgumentNullException(nameof(repos));
        _knowledge = knowledge ?? throw new ArgumentNullException(nameof(knowledge));
        _connectorFactory = connectorFactory ?? throw new ArgumentNullException(nameof(connectorFactory));
    }

    /// <summary>The repos this service is configured to ingest (a read-only view for the admin API).</summary>
    public IReadOnlyList<RepoSpec> ConfiguredRepos => _repos;

    /// <summary>Re-ingest every configured repo, returning a per-repo result (errors captured, not thrown).</summary>
    public async Task<IReadOnlyList<RepoIngestResult>> ReindexAllAsync(CancellationToken cancellationToken = default)
    {
        var results = new List<RepoIngestResult>(_repos.Count);
        foreach (var spec in _repos)
        {
            var pipeline = new IngestPipeline(_knowledge.WithAcl(DocumentAcl.ForGroups(spec.AclGroup)));
            try
            {
                var result = await pipeline.IngestAsync(_connectorFactory(spec), cancellationToken).ConfigureAwait(false);
                results.Add(new RepoIngestResult(spec.Slug, result.Documents, result.Chunks, null));
            }
            catch (Exception ex)
            {
                results.Add(new RepoIngestResult(spec.Slug, 0, 0, ex.Message));
            }
        }
        return results;
    }
}
