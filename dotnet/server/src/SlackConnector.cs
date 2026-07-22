using System.Globalization;
using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Pulls messages from a Slack workspace via the Web API — the C# analog of a SaaS chat connector.
/// Resolves author names once (<c>users.list</c>), lists channels (<c>conversations.list</c>), then
/// pages each channel's messages (<c>conversations.history</c>). Emits <b>one document per channel
/// per day</b> with a stable id <c>slack:{channel}:{date}</c>: today's document re-hashes on every
/// pull as new messages land, while completed past days produce identical content and dedupe on the
/// pipeline's (id, hash) key. <see cref="SourceDocument.Source"/> is the permalink of the day's first
/// message, and each document carries a per-channel ACL label (<c>slack:channel:{id}</c>).
///
/// The caller supplies a configured <see cref="HttpClient"/> (bot token as
/// <c>Authorization: Bearer xoxb-…</c>). Network parsing is unit-tested against a fake handler, so
/// the connector logic runs in CI without hitting Slack. Threaded replies (<c>conversations.replies</c>)
/// are deferred — only channel-level messages are ingested for now.
/// </summary>
public sealed class SlackConnector : IConnector
{
    private const string ApiBase = "https://slack.com/api";
    private const int PageLimit = 200;

    private static readonly JsonSerializerOptions JsonOptions = new() { PropertyNameCaseInsensitive = true };

    private readonly HttpClient _http;
    private readonly DateTimeOffset? _oldest;
    private readonly string _channelTypes;

    /// <param name="httpClient">Configured with the Slack bot token as a Bearer Authorization header.</param>
    /// <param name="oldest">Incremental cursor — only pull messages at/after this instant. Null pulls all history.</param>
    /// <param name="channelTypes">Slack <c>conversations.list</c> <c>types</c> filter (default public channels).</param>
    public SlackConnector(HttpClient httpClient, DateTimeOffset? oldest = null, string channelTypes = "public_channel")
    {
        _http = httpClient ?? throw new ArgumentNullException(nameof(httpClient));
        _oldest = oldest;
        _channelTypes = channelTypes;
    }

    public async IAsyncEnumerable<SourceDocument> PullAsync([EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        var users = await LoadUsersAsync(cancellationToken).ConfigureAwait(false);

        await foreach (var channel in ListChannelsAsync(cancellationToken).ConfigureAwait(false))
        {
            // Group this channel's messages by UTC day. Slack returns history newest-first; we sort
            // each day's messages ascending so the "first message" (permalink source) is the earliest.
            var byDay = new Dictionary<string, List<SlackMessage>>(StringComparer.Ordinal);
            await foreach (var message in HistoryAsync(channel.Id!, cancellationToken).ConfigureAwait(false))
            {
                if (message.Type != "message" || string.IsNullOrEmpty(message.Ts) || string.IsNullOrEmpty(message.Text))
                {
                    continue;
                }
                var day = DayOf(message.Ts!);
                if (day is null)
                {
                    continue;
                }
                if (!byDay.TryGetValue(day, out var bucket))
                {
                    byDay[day] = bucket = new List<SlackMessage>();
                }
                bucket.Add(message);
            }

            foreach (var (day, messages) in byDay)
            {
                messages.Sort(static (a, b) => string.CompareOrdinal(a.Ts, b.Ts));
                var first = messages[0];
                var source = await PermalinkAsync(channel.Id!, first.Ts!, cancellationToken).ConfigureAwait(false)
                    ?? $"slack://{channel.Id}/{first.Ts}";

                yield return new SourceDocument(
                    Id: $"slack:{channel.Id}:{day}",
                    Source: source,
                    Content: RenderDay(channel.Name ?? channel.Id!, day, messages, users),
                    DocType: DocumentType.Documentation,
                    Acl: new[] { $"slack:channel:{channel.Id}" });
            }
        }
    }

    private static string RenderDay(string channelName, string day, List<SlackMessage> messages, IReadOnlyDictionary<string, string> users)
    {
        var sb = new StringBuilder();
        sb.Append("# #").Append(channelName).Append(" — ").Append(day).Append('\n').Append('\n');
        foreach (var message in messages)
        {
            var author = message.User is not null && users.TryGetValue(message.User, out var name) ? name : message.User ?? "unknown";
            sb.Append(author).Append(": ").Append(message.Text).Append('\n');
        }
        return sb.ToString();
    }

    /// <summary>UTC calendar day (yyyy-MM-dd) for a Slack ts ("secs.micros"), or null if unparseable.</summary>
    private static string? DayOf(string ts)
    {
        var dot = ts.IndexOf('.');
        var whole = dot >= 0 ? ts[..dot] : ts;
        if (!long.TryParse(whole, NumberStyles.Integer, CultureInfo.InvariantCulture, out var seconds))
        {
            return null;
        }
        return DateTimeOffset.FromUnixTimeSeconds(seconds).UtcDateTime.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture);
    }

    private async Task<IReadOnlyDictionary<string, string>> LoadUsersAsync(CancellationToken cancellationToken)
    {
        var users = new Dictionary<string, string>(StringComparer.Ordinal);
        string? cursor = null;
        do
        {
            var url = $"{ApiBase}/users.list?limit={PageLimit}" + (string.IsNullOrEmpty(cursor) ? "" : $"&cursor={Uri.EscapeDataString(cursor)}");
            var response = await GetAsync<UsersListResponse>(url, cancellationToken).ConfigureAwait(false);
            foreach (var member in response.Members ?? new List<SlackUser>())
            {
                if (member.Id is null)
                {
                    continue;
                }
                // Prefer real name, then profile display name, then handle, then id.
                users[member.Id] = Coalesce(member.RealName, member.Profile?.DisplayName, member.Profile?.RealName, member.Name) ?? member.Id;
            }
            cursor = response.ResponseMetadata?.NextCursor;
        }
        while (!string.IsNullOrEmpty(cursor));
        return users;
    }

    private async IAsyncEnumerable<SlackChannel> ListChannelsAsync([EnumeratorCancellation] CancellationToken cancellationToken)
    {
        string? cursor = null;
        do
        {
            var url = $"{ApiBase}/conversations.list?limit={PageLimit}&types={Uri.EscapeDataString(_channelTypes)}"
                + (string.IsNullOrEmpty(cursor) ? "" : $"&cursor={Uri.EscapeDataString(cursor)}");
            var response = await GetAsync<ConversationsListResponse>(url, cancellationToken).ConfigureAwait(false);
            foreach (var channel in response.Channels ?? new List<SlackChannel>())
            {
                if (channel.Id is not null)
                {
                    yield return channel;
                }
            }
            cursor = response.ResponseMetadata?.NextCursor;
        }
        while (!string.IsNullOrEmpty(cursor));
    }

    private async IAsyncEnumerable<SlackMessage> HistoryAsync(string channelId, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        string? cursor = null;
        // Incremental pull: only messages at/after the configured `oldest` instant.
        var oldest = _oldest is { } o ? o.ToUnixTimeSeconds().ToString(CultureInfo.InvariantCulture) : null;
        do
        {
            var url = $"{ApiBase}/conversations.history?channel={Uri.EscapeDataString(channelId)}&limit={PageLimit}"
                + (oldest is null ? "" : $"&oldest={oldest}")
                + (string.IsNullOrEmpty(cursor) ? "" : $"&cursor={Uri.EscapeDataString(cursor)}");
            var response = await GetAsync<ConversationsHistoryResponse>(url, cancellationToken).ConfigureAwait(false);
            foreach (var message in response.Messages ?? new List<SlackMessage>())
            {
                yield return message;
            }
            cursor = response.ResponseMetadata?.NextCursor;
        }
        while (!string.IsNullOrEmpty(cursor));
    }

    private async Task<string?> PermalinkAsync(string channelId, string ts, CancellationToken cancellationToken)
    {
        var url = $"{ApiBase}/chat.getPermalink?channel={Uri.EscapeDataString(channelId)}&message_ts={Uri.EscapeDataString(ts)}";
        // A permalink failure shouldn't sink the whole pull — fall back to a synthetic source upstream.
        using var response = await _http.GetAsync(url, cancellationToken).ConfigureAwait(false);
        if (!response.IsSuccessStatusCode)
        {
            return null;
        }
        await using var stream = await response.Content.ReadAsStreamAsync(cancellationToken).ConfigureAwait(false);
        var body = await JsonSerializer.DeserializeAsync<PermalinkResponse>(stream, JsonOptions, cancellationToken).ConfigureAwait(false);
        return body is { Ok: true } ? body.Permalink : null;
    }

    /// <summary>GET + deserialize, failing loud on transport errors and Slack <c>ok:false</c> responses.</summary>
    private async Task<T> GetAsync<T>(string url, CancellationToken cancellationToken)
        where T : SlackResponse
    {
        using var response = await _http.GetAsync(url, cancellationToken).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();
        await using var stream = await response.Content.ReadAsStreamAsync(cancellationToken).ConfigureAwait(false);
        var body = await JsonSerializer.DeserializeAsync<T>(stream, JsonOptions, cancellationToken).ConfigureAwait(false)
            ?? throw new InvalidOperationException($"Slack returned an empty body for {url}");
        if (!body.Ok)
        {
            // Fail loud like the GitHub connector's truncated-tree guard: a silent partial pull would
            // index an incomplete workspace and report success.
            throw new InvalidOperationException($"Slack API error for {url}: {body.Error ?? "unknown"}");
        }
        return body;
    }

    private static string? Coalesce(params string?[] values)
    {
        foreach (var value in values)
        {
            if (!string.IsNullOrWhiteSpace(value))
            {
                return value;
            }
        }
        return null;
    }

    private abstract record SlackResponse
    {
        [JsonPropertyName("ok")]
        public bool Ok { get; init; }

        [JsonPropertyName("error")]
        public string? Error { get; init; }

        [JsonPropertyName("response_metadata")]
        public ResponseMetadata? ResponseMetadata { get; init; }
    }

    private sealed record ResponseMetadata([property: JsonPropertyName("next_cursor")] string? NextCursor);

    private sealed record UsersListResponse : SlackResponse
    {
        [JsonPropertyName("members")]
        public List<SlackUser>? Members { get; init; }
    }

    private sealed record ConversationsListResponse : SlackResponse
    {
        [JsonPropertyName("channels")]
        public List<SlackChannel>? Channels { get; init; }
    }

    private sealed record ConversationsHistoryResponse : SlackResponse
    {
        [JsonPropertyName("messages")]
        public List<SlackMessage>? Messages { get; init; }
    }

    private sealed record PermalinkResponse : SlackResponse
    {
        [JsonPropertyName("permalink")]
        public string? Permalink { get; init; }
    }

    private sealed record SlackUser(
        [property: JsonPropertyName("id")] string? Id,
        [property: JsonPropertyName("name")] string? Name,
        [property: JsonPropertyName("real_name")] string? RealName,
        [property: JsonPropertyName("profile")] SlackUserProfile? Profile);

    private sealed record SlackUserProfile(
        [property: JsonPropertyName("real_name")] string? RealName,
        [property: JsonPropertyName("display_name")] string? DisplayName);

    private sealed record SlackChannel(
        [property: JsonPropertyName("id")] string? Id,
        [property: JsonPropertyName("name")] string? Name);

    private sealed record SlackMessage(
        [property: JsonPropertyName("type")] string? Type,
        [property: JsonPropertyName("user")] string? User,
        [property: JsonPropertyName("text")] string? Text,
        [property: JsonPropertyName("ts")] string? Ts);
}
