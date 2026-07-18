using System.Runtime.CompilerServices;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// A raw document pulled from a source, before chunking. <c>Acl</c> holds optional per-document
/// access-control labels (entitlement groups, e.g. <c>slack:channel:C123</c>) — mirrors the Rust
/// <c>RawDocument.acl</c>. Carried through for ACL-filtered retrieval; a connector whose documents
/// have differing access (Slack's per-channel case) stamps them here rather than relying on a single
/// pipeline-wide ACL.
/// </summary>
public sealed record SourceDocument(string Id, string Source, string Content, DocumentType DocType = DocumentType.Documentation, IReadOnlyList<string>? Acl = null);

/// <summary>
/// A knowledge source the ingest pipeline pulls documents from (GitHub, files, …). Mirrors the
/// Rust engine's <c>Connector</c> trait. Streamed so large sources don't materialize at once.
/// </summary>
public interface IConnector
{
    IAsyncEnumerable<SourceDocument> PullAsync(CancellationToken cancellationToken = default);
}

/// <summary>
/// A scripted <see cref="IConnector"/> — the C# analog of the Rust <c>MockConnector</c>. The
/// connector contract is asserted against this first (CI-safe), before any real connector.
/// </summary>
public sealed class MockConnector : IConnector
{
    private readonly IReadOnlyList<SourceDocument> _documents;

    public MockConnector(params SourceDocument[] documents) => _documents = documents;

    public async IAsyncEnumerable<SourceDocument> PullAsync([EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        foreach (var document in _documents)
        {
            await Task.Yield();
            yield return document;
        }
    }
}
