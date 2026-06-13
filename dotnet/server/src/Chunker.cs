namespace SmooAI.SmoothOperator.Server;

/// <summary>How to split a document into chunks for embedding/retrieval.</summary>
public sealed record ChunkingOptions(int MaxChars = 1200, int OverlapChars = 150);

/// <summary>
/// Splits a document into overlapping, size-bounded chunks, preferring to break at whitespace.
/// The C# analog of the Rust engine's chunking pipeline (the G2 gap). Each chunk carries enough
/// overlap that a fact spanning a boundary is still retrievable from one side.
/// </summary>
public static class Chunker
{
    public static IReadOnlyList<string> Chunk(string content, ChunkingOptions options)
    {
        var text = content.Trim();
        if (text.Length == 0)
        {
            return Array.Empty<string>();
        }
        if (text.Length <= options.MaxChars)
        {
            return new[] { text };
        }
        if (options.OverlapChars >= options.MaxChars)
        {
            throw new ArgumentException("OverlapChars must be smaller than MaxChars.", nameof(options));
        }

        var chunks = new List<string>();
        var start = 0;
        while (start < text.Length)
        {
            var end = Math.Min(start + options.MaxChars, text.Length);
            if (end < text.Length)
            {
                // Prefer to break at the last whitespace within this window — but ONLY if that break
                // still leaves room to advance past the overlap. Otherwise (e.g. a long run of
                // non-whitespace like minified code or a base64 blob, where the only space is near
                // the window start) breaking there would set the next start = end - overlap BACKWARD,
                // and the loop would never progress. In that case keep the hard cut at start+MaxChars,
                // which always advances since MaxChars > OverlapChars.
                var window = end - start;
                var whitespace = text.LastIndexOf(' ', end - 1, window);
                if (whitespace > start + options.OverlapChars)
                {
                    end = whitespace;
                }
            }

            var piece = text[start..end].Trim();
            if (piece.Length > 0)
            {
                chunks.Add(piece);
            }

            if (end >= text.Length)
            {
                break;
            }

            // Advance with overlap, but never regress or stall — guarantee forward progress so the
            // loop always terminates regardless of where the break landed.
            var nextStart = end - options.OverlapChars;
            start = nextStart > start ? nextStart : end;
        }

        return chunks;
    }
}
