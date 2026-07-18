using System.Text;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Durable dedup state for idempotent ingest — the C# analog of the Rust ingestion <c>IngestLedger</c>.
/// Holds the set of <c>{documentId, contentHash}</c> keys already stored so re-ingesting identical
/// content is a no-op, while changed content (a new hash) is reprocessed. The engine's knowledge base
/// exposes no list/delete, so idempotency is the ingestion layer's responsibility; this ledger is that
/// memory. Thread-safe and cheap to share across ingest runs (a production deployment would persist it
/// alongside the knowledge store).
/// </summary>
public sealed class IngestLedger
{
    private readonly HashSet<string> _seen = new();
    private readonly object _gate = new();

    /// <summary>Number of distinct <c>{doc, hash}</c> keys recorded.</summary>
    public int Count
    {
        get
        {
            lock (_gate)
            {
                return _seen.Count;
            }
        }
    }

    /// <summary>Whether the ledger has recorded anything.</summary>
    public bool IsEmpty => Count == 0;

    /// <summary>The dedup key for a chunk: <c>"{documentId}::{contentHash}"</c>.</summary>
    public static string KeyFor(string documentId, string text) => $"{documentId}::{ContentHash(text)}";

    /// <summary>Non-recording membership probe — true if this exact key was already recorded.</summary>
    public bool Contains(string key)
    {
        lock (_gate)
        {
            return _seen.Contains(key);
        }
    }

    /// <summary>Record a key; returns <c>true</c> if it was newly inserted (not seen before).</summary>
    public bool Record(string key)
    {
        lock (_gate)
        {
            return _seen.Add(key);
        }
    }

    /// <summary>
    /// FNV-1a 64-bit hash of the text (UTF-8 bytes), hex-encoded to 16 digits. Stable across runs and
    /// platforms, and byte-for-byte identical to the Rust ledger's <c>content_hash</c> so a chunk hashes
    /// the same in either language.
    /// </summary>
    public static string ContentHash(string text)
    {
        // unchecked so the multiply wraps (matching Rust's wrapping_mul) regardless of build settings.
        unchecked
        {
            ulong hash = 0xcbf2_9ce4_8422_2325;
            foreach (var b in Encoding.UTF8.GetBytes(text))
            {
                hash ^= b;
                hash *= 0x0000_0100_0000_01b3;
            }
            return hash.ToString("x16");
        }
    }
}
