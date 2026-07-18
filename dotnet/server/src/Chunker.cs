using System.Text;

namespace SmooAI.SmoothOperator.Server;

/// <summary>How to split a document into chunks for embedding/retrieval.</summary>
/// <remarks>
/// Defaults match the Rust ingestion chunker: ~500-char paragraph-aware chunks with 64 chars of
/// whole-word overlap. <see cref="OverlapChars"/> is clamped below <see cref="MaxChars"/> at split
/// time so a chunk always makes forward progress (an overlap ≥ the cap would loop forever).
/// </remarks>
public sealed record ChunkingOptions(int MaxChars = 500, int OverlapChars = 64);

/// <summary>One chunk produced from a document — a stable id plus its text.</summary>
/// <param name="Id">Stable id: <c>"{documentId}#{index}"</c>.</param>
/// <param name="DocumentId">The originating document's id.</param>
/// <param name="Index">0-based position within the document.</param>
/// <param name="Text">The chunk text.</param>
public sealed record Chunk(string Id, string DocumentId, int Index, string Text);

/// <summary>
/// Splits a document into overlapping, size-bounded chunks. The C# analog of the Rust ingestion
/// chunker (the G2 gap). Strategy: split on blank lines into paragraph units, hard-split any unit
/// larger than the cap on word boundaries, greedily pack units up to <see cref="ChunkingOptions.MaxChars"/>,
/// then carry <see cref="ChunkingOptions.OverlapChars"/> of trailing whole words into the next chunk
/// so a fact spanning a boundary stays retrievable. Each chunk gets a stable <c>"{documentId}#{index}"</c> id.
/// </summary>
public static class Chunker
{
    /// <summary>Split raw content into chunk-sized texts (no ids; pure string work).</summary>
    public static IReadOnlyList<string> Chunk(string content, ChunkingOptions options) => SplitText(content, options);

    /// <summary>Chunk a document into ordered <see cref="T:SmooAI.SmoothOperator.Server.Chunk"/> items with stable <c>"{documentId}#{index}"</c> ids.</summary>
    public static IReadOnlyList<Chunk> Chunk(SourceDocument doc, ChunkingOptions options)
    {
        var texts = SplitText(doc.Content, options);
        var chunks = new List<Chunk>(texts.Count);
        for (var i = 0; i < texts.Count; i++)
        {
            chunks.Add(new Chunk($"{doc.Id}#{i}", doc.Id, i, texts[i]));
        }
        return chunks;
    }

    private static IReadOnlyList<string> SplitText(string content, ChunkingOptions options)
    {
        var maxChars = Math.Max(1, options.MaxChars);
        // Clamp overlap below the cap so a chunk always makes forward progress (mirrors Rust's Chunker::new).
        var overlapChars = Math.Clamp(options.OverlapChars, 0, maxChars - 1);

        // 1. Paragraph units (blank-line separated), oversized ones hard-split on word boundaries.
        var units = new List<string>();
        foreach (var para in content.Split("\n\n"))
        {
            var trimmed = para.Trim();
            if (trimmed.Length == 0)
            {
                continue;
            }
            if (trimmed.Length <= maxChars)
            {
                units.Add(trimmed);
            }
            else
            {
                units.AddRange(HardSplitWords(trimmed, maxChars));
            }
        }

        // 2. Greedily pack units (joined by a blank line) up to the cap.
        var chunks = new List<string>();
        var current = new StringBuilder();
        foreach (var unit in units)
        {
            if (current.Length == 0)
            {
                current.Append(unit);
            }
            else if (current.Length + 2 + unit.Length <= maxChars)
            {
                current.Append("\n\n").Append(unit);
            }
            else
            {
                chunks.Add(current.ToString());
                current.Clear();
                current.Append(unit);
            }
        }
        if (current.Length > 0)
        {
            chunks.Add(current.ToString());
        }

        // 3. Prepend trailing whole-word overlap onto each successive chunk.
        return ApplyOverlap(chunks, overlapChars);
    }

    /// <summary>Hard-split a single oversized paragraph at word boundaries.</summary>
    private static IReadOnlyList<string> HardSplitWords(string para, int maxChars)
    {
        var output = new List<string>();
        var current = new StringBuilder();
        foreach (var word in para.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
        {
            if (current.Length == 0)
            {
                current.Append(word);
            }
            else if (current.Length + 1 + word.Length > maxChars)
            {
                output.Add(current.ToString());
                current.Clear();
                current.Append(word);
            }
            else
            {
                current.Append(' ').Append(word);
            }
        }
        if (current.Length > 0)
        {
            output.Add(current.ToString());
        }
        return output;
    }

    /// <summary>Prepend the trailing <paramref name="overlapChars"/> (rounded to whole words) of each chunk onto the next.</summary>
    private static IReadOnlyList<string> ApplyOverlap(List<string> chunks, int overlapChars)
    {
        if (overlapChars == 0 || chunks.Count < 2)
        {
            return chunks;
        }
        var output = new List<string>(chunks.Count);
        for (var i = 0; i < chunks.Count; i++)
        {
            if (i == 0)
            {
                output.Add(chunks[i]);
                continue;
            }
            var tail = TrailingWords(chunks[i - 1], overlapChars);
            output.Add(tail.Length == 0 ? chunks[i] : $"{tail} {chunks[i]}");
        }
        return output;
    }

    /// <summary>The last whole words of <paramref name="s"/> totaling at most <paramref name="overlapChars"/> characters.</summary>
    private static string TrailingWords(string s, int overlapChars)
    {
        var words = s.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries);
        var take = 0;
        var len = 0;
        for (var i = words.Length - 1; i >= 0; i--)
        {
            var add = words[i].Length + (take > 0 ? 1 : 0);
            if (len + add > overlapChars)
            {
                break;
            }
            len += add;
            take++;
        }
        return take == 0 ? string.Empty : string.Join(' ', words, words.Length - take, take);
    }
}
