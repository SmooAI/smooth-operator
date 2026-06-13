using System.Runtime.CompilerServices;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// The repo-ingestion service: spec parsing, ACL-group stamping (so retrieval stays ACL-scoped),
/// and per-repo error capture (one repo failing must not abort the run). The connector is injected,
/// so these run with a <see cref="MockConnector"/> — no GitHub, CI-safe.
/// </summary>
public class RepoIngestionServiceTests
{
    [Theory]
    [InlineData("acme/widgets", "acme", "widgets", "main")]
    [InlineData("acme/widgets@dev", "acme", "widgets", "dev")]
    [InlineData("acme/widgets@release/1.2", "acme", "widgets", "release/1.2")]
    public void RepoSpec_Parse(string spec, string owner, string repo, string gitRef)
    {
        var parsed = RepoSpec.Parse(spec);
        Assert.Equal(owner, parsed.Owner);
        Assert.Equal(repo, parsed.Repo);
        Assert.Equal(gitRef, parsed.GitRef);
        Assert.Equal($"{owner}/{repo}", parsed.Slug);
        Assert.Equal($"github:{owner}/{repo}", parsed.AclGroup);
    }

    [Fact]
    public async Task ReindexAll_IngestsUnderRepoAclGroup()
    {
        var kb = new AclKnowledgeStore();
        var service = new RepoIngestionService(
            new[] { new RepoSpec("acme", "docs", "main") },
            kb,
            _ => new MockConnector(new SourceDocument("d1", "runbook.md", "The deploy runbook lives here.")));

        var results = await service.ReindexAllAsync();

        Assert.Single(results);
        Assert.True(results[0].Ok);
        Assert.Equal("acme/docs", results[0].Repo);
        Assert.True(results[0].Documents >= 1);

        // The ingested doc is entitled to github:acme/docs — an entitled user retrieves it; anonymous can't.
        var entitled = new AccessContext(new Principal("u", "acme", "basic", new[] { "github:acme/docs" }), IsAnonymous: false);
        var entitledHits = await kb.ForAccess(entitled)!.QueryAsync("deploy runbook", 5);
        Assert.Contains(entitledHits, h => h.Source == "runbook.md");

        var anonHits = await kb.ForAccess(AccessContext.Anonymous)!.QueryAsync("deploy runbook", 5);
        Assert.DoesNotContain(anonHits, h => h.Source == "runbook.md");
    }

    [Fact]
    public async Task ReindexAll_CapturesPerRepoError_WithoutAborting()
    {
        var kb = new AclKnowledgeStore();
        var service = new RepoIngestionService(
            new[] { new RepoSpec("bad", "repo", "main"), new RepoSpec("good", "repo", "main") },
            kb,
            spec => spec.Owner == "bad" ? new ThrowingConnector() : new MockConnector(new SourceDocument("d", "ok.md", "fine")));

        var results = await service.ReindexAllAsync();

        Assert.Equal(2, results.Count);
        Assert.False(results[0].Ok);          // bad/repo's error is captured…
        Assert.NotNull(results[0].Error);
        Assert.True(results[1].Ok);           // …and the next repo still ingests
    }

    [Fact]
    public void ConfiguredRepos_ExposesTheSpecs()
    {
        var specs = new[] { new RepoSpec("a", "b", "main"), new RepoSpec("c", "d", "dev") };
        var service = new RepoIngestionService(specs, new AclKnowledgeStore(), _ => new MockConnector());
        Assert.Equal(new[] { "a/b", "c/d" }, service.ConfiguredRepos.Select(r => r.Slug));
    }

    private sealed class ThrowingConnector : IConnector
    {
        public IAsyncEnumerable<SourceDocument> PullAsync(CancellationToken cancellationToken = default) =>
            throw new InvalidOperationException("boom");
    }
}
