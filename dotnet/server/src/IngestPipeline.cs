using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>What an ingest run produced.</summary>
/// <param name="Documents">Documents the connector returned.</param>
/// <param name="Chunks">Chunks newly stored this run.</param>
/// <param name="SkippedDocuments">Documents skipped as unchanged (every chunk already in the ledger).</param>
public sealed record IngestResult(int Documents, int Chunks, int SkippedDocuments = 0);

/// <summary>
/// Pulls documents from an <see cref="IConnector"/>, chunks them, and ingests each chunk into an
/// <see cref="IKnowledgeBase"/> (which embeds + stores). The C# analog of the Rust ingest pipeline:
/// connector → chunk → embed → store. Idempotent on <c>(documentId, contentHash)</c> via the
/// <see cref="IngestLedger"/>: re-ingesting identical content is a no-op, changed content is reprocessed.
/// After a run, the source's content is retrievable.
/// </summary>
public sealed class IngestPipeline
{
    private readonly IKnowledgeBase _knowledge;
    private readonly ChunkingOptions _chunking;
    private readonly IngestLedger _ledger;

    public IngestPipeline(IKnowledgeBase knowledge, ChunkingOptions? chunking = null, IngestLedger? ledger = null)
    {
        _knowledge = knowledge ?? throw new ArgumentNullException(nameof(knowledge));
        _chunking = chunking ?? new ChunkingOptions();
        // A fresh ledger per pipeline unless a shared one is injected — share it across runs for
        // cross-run idempotency (mirrors the Rust IngestOptions::with_ledger builder).
        _ledger = ledger ?? new IngestLedger();
    }

    public async Task<IngestResult> IngestAsync(IConnector connector, CancellationToken cancellationToken = default)
    {
        var documents = 0;
        var chunks = 0;
        var skippedDocuments = 0;

        await foreach (var document in connector.PullAsync(cancellationToken).ConfigureAwait(false))
        {
            documents++;
            var pieces = Chunker.Chunk(document, _chunking);
            if (pieces.Count == 0)
            {
                continue;
            }

            // Idempotency: a document is "unchanged" when every chunk it produces is already recorded
            // under (documentId, chunk content hash). Probe without recording; skip the whole doc if so.
            var keys = new string[pieces.Count];
            var anyNew = false;
            for (var i = 0; i < pieces.Count; i++)
            {
                keys[i] = IngestLedger.KeyFor(document.Id, pieces[i].Text);
                anyNew |= !_ledger.Contains(keys[i]);
            }
            if (!anyNew)
            {
                skippedDocuments++;
                continue;
            }

            for (var i = 0; i < pieces.Count; i++)
            {
                // Record returns false if this exact (doc, hash) was already stored — stay idempotent.
                if (!_ledger.Record(keys[i]))
                {
                    continue;
                }
                await _knowledge.IngestAsync(
                    new KnowledgeDocument(pieces[i].Id, pieces[i].Text, document.Source, document.DocType),
                    cancellationToken).ConfigureAwait(false);
                chunks++;
            }
        }

        return new IngestResult(documents, chunks, skippedDocuments);
    }
}
