using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// One configured Notion root: a page whose subtree is ingested, tagged with the entitlement
/// <see cref="AclLabels"/> that every document pulled from it (the root page and all of its descendant
/// child pages) is stamped onto <see cref="SourceDocument.Acl"/>. Lets one connector span several
/// roots with different access. Mirrors the Slack connector's per-channel <c>slack:channel:{id}</c>
/// labels; a Notion root's default label is <c>notion:root:{pageId}</c> when none is supplied.
/// </summary>
public sealed record NotionRoot(string PageId, IReadOnlyList<string> AclLabels)
{
    /// <summary>A root labelled with the default <c>notion:root:{canonicalPageId}</c> entitlement.</summary>
    public static NotionRoot WithDefaultLabel(string pageId) =>
        new(pageId, new[] { $"notion:root:{pageId.Replace("-", string.Empty, StringComparison.Ordinal)}" });
}

/// <summary>
/// Pulls documents from Notion — the C# analog of the Rust engine's connector trait, in the shape of
/// <see cref="GitHubConnector"/>. For each configured <see cref="NotionRoot"/> it recurses
/// <c>blocks/{id}/children</c> (paginating), flattens the text-bearing block types into the page's
/// document text, and emits a <see cref="SourceDocument"/> per page. A <c>child_page</c> block is NOT
/// inlined — it becomes its own recursed document (under the same root's ACL), so citations resolve to
/// the page a passage actually lives on. The document id is the Notion page id and the source is the
/// page URL, so citations link back and re-ingesting the same page overwrites rather than duplicates.
///
/// <para>The caller supplies a configured <see cref="HttpClient"/> whose <c>Authorization</c> header
/// carries the integration token (<c>Bearer secret_…</c>); the connector adds the required
/// <c>Notion-Version</c> header per request. Network parsing is unit-tested against a fake handler, so
/// the connector logic runs in CI without hitting Notion.</para>
/// </summary>
public sealed class NotionConnector : IConnector
{
    // The pinned REST version. Notion requires this header on every request; unversioned requests are
    // rejected, and a newer version can change response shapes, so we pin the one this code parses.
    private const string NotionVersion = "2022-06-28";
    private const string ApiBase = "https://api.notion.com/v1";

    // Block types whose rich_text we flatten into the page's document text.
    private static readonly HashSet<string> TextBlockTypes = new(StringComparer.Ordinal)
    {
        "paragraph", "heading_1", "heading_2", "heading_3",
        "bulleted_list_item", "numbered_list_item", "quote", "code", "toggle",
    };

    private readonly HttpClient _http;
    private readonly IReadOnlyList<NotionRoot> _roots;

    public NotionConnector(IEnumerable<NotionRoot> roots, HttpClient httpClient)
    {
        _http = httpClient ?? throw new ArgumentNullException(nameof(httpClient));
        _roots = roots?.ToArray() ?? throw new ArgumentNullException(nameof(roots));
    }

    public async IAsyncEnumerable<SourceDocument> PullAsync([EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        // Work queue of pages to emit; child_page blocks enqueue more as they're discovered. `seen`
        // (canonical, dash-free id) guards against a page reachable by two paths / a cyclic link.
        var queue = new Queue<(string PageId, IReadOnlyList<string> Acl)>();
        foreach (var root in _roots)
        {
            queue.Enqueue((root.PageId, root.AclLabels));
        }
        var seen = new HashSet<string>(StringComparer.Ordinal);

        while (queue.Count > 0)
        {
            var (pageId, acl) = queue.Dequeue();
            var canonical = CanonicalId(pageId);
            if (!seen.Add(canonical))
            {
                continue;
            }

            var text = new StringBuilder();
            await AppendChildrenAsync(pageId, acl, text, queue, cancellationToken).ConfigureAwait(false);

            yield return new SourceDocument(
                Id: canonical,
                Source: PageUrl(canonical),
                Content: text.ToString().TrimEnd(),
                DocType: DocumentType.Documentation,
                Acl: acl);
        }
    }

    /// <summary>
    /// Recurse <c>blocks/{blockId}/children</c> (paginated), appending flattened text for text-bearing
    /// blocks and their nested children, and enqueueing each <c>child_page</c> as a separate document
    /// rather than inlining it.
    /// </summary>
    private async Task AppendChildrenAsync(string blockId, IReadOnlyList<string> acl, StringBuilder text, Queue<(string, IReadOnlyList<string>)> queue, CancellationToken cancellationToken)
    {
        string? cursor = null;
        do
        {
            var url = $"{ApiBase}/blocks/{CanonicalId(blockId)}/children?page_size=100";
            if (cursor is not null)
            {
                url += $"&start_cursor={Uri.EscapeDataString(cursor)}";
            }

            using var request = new HttpRequestMessage(HttpMethod.Get, url);
            request.Headers.Add("Notion-Version", NotionVersion);

            using var response = await _http.SendAsync(request, cancellationToken).ConfigureAwait(false);
            response.EnsureSuccessStatusCode();
            await using var stream = await response.Content.ReadAsStreamAsync(cancellationToken).ConfigureAwait(false);
            using var doc = await JsonDocument.ParseAsync(stream, cancellationToken: cancellationToken).ConfigureAwait(false);
            var root = doc.RootElement;

            if (root.TryGetProperty("results", out var results) && results.ValueKind == JsonValueKind.Array)
            {
                foreach (var block in results.EnumerateArray())
                {
                    await ProcessBlockAsync(block, acl, text, queue, cancellationToken).ConfigureAwait(false);
                }
            }

            cursor = root.TryGetProperty("has_more", out var hasMore) && hasMore.ValueKind == JsonValueKind.True
                && root.TryGetProperty("next_cursor", out var next) && next.ValueKind == JsonValueKind.String
                ? next.GetString()
                : null;
        }
        while (cursor is not null);
    }

    private async Task ProcessBlockAsync(JsonElement block, IReadOnlyList<string> acl, StringBuilder text, Queue<(string, IReadOnlyList<string>)> queue, CancellationToken cancellationToken)
    {
        if (!block.TryGetProperty("type", out var typeElement) || typeElement.GetString() is not { } type)
        {
            return;
        }

        // A child_page block's id IS the child page's id. It becomes its own document (same root ACL),
        // never inlined — so we do NOT recurse its children here.
        if (type == "child_page")
        {
            if (block.TryGetProperty("id", out var childId) && childId.GetString() is { } id)
            {
                queue.Enqueue((id, acl));
            }
            return;
        }

        if (TextBlockTypes.Contains(type) && block.TryGetProperty(type, out var payload))
        {
            var line = FlattenRichText(payload);
            if (line.Length > 0)
            {
                text.Append(line).Append('\n');
            }
        }

        // Nested content (toggle bodies, list-item sub-items, …) lives under the block's own children.
        if (block.TryGetProperty("has_children", out var hasChildren) && hasChildren.ValueKind == JsonValueKind.True
            && block.TryGetProperty("id", out var blockId) && blockId.GetString() is { } childrenOf)
        {
            await AppendChildrenAsync(childrenOf, acl, text, queue, cancellationToken).ConfigureAwait(false);
        }
    }

    /// <summary>Concatenate the <c>plain_text</c> of a block payload's <c>rich_text</c> array.</summary>
    private static string FlattenRichText(JsonElement payload)
    {
        if (!payload.TryGetProperty("rich_text", out var richText) || richText.ValueKind != JsonValueKind.Array)
        {
            return string.Empty;
        }
        var builder = new StringBuilder();
        foreach (var span in richText.EnumerateArray())
        {
            if (span.TryGetProperty("plain_text", out var plain) && plain.GetString() is { } value)
            {
                builder.Append(value);
            }
        }
        return builder.ToString();
    }

    /// <summary>Notion page/block ids are UUIDs; strip dashes so the id is stable regardless of the
    /// dashed/dashless form the caller or API hands back, and so it matches the URL form.</summary>
    private static string CanonicalId(string id) => id.Replace("-", string.Empty, StringComparison.Ordinal);

    private static string PageUrl(string canonicalId) => $"https://www.notion.so/{canonicalId}";
}
