namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// A conversation session: the unit the protocol's create/get operate on.
/// <para>
/// <c>AgentId</c> is null when the caller named no agent. Nullable on purpose: it used to be filled
/// with a fresh GUID, which pointed every agentless session at an agent that never existed —
/// silently, since that id then flowed into the participant's internal_id and the per-agent config
/// lookup and resolved to nothing. Absent is now absent. th-68897a.
/// </para>
/// </summary>
public sealed record StoredSession(
    string SessionId,
    string ConversationId,
    string? AgentId,
    string AgentName,
    string UserParticipantId,
    string AgentParticipantId,
    string? UserEmail = null);

public enum MessageDirection
{
    /// <summary>From the user.</summary>
    Inbound,

    /// <summary>From the agent.</summary>
    Outbound,
}

/// <summary>
/// One logged conversation message. <see cref="CreatedAt"/> is an init-only property (not a
/// positional parameter) on purpose: it was added for <c>get_conversation_messages</c> paging and a
/// positional param would break every downstream host that constructs a <c>StoredMessage</c>. Stores
/// that persist a real timestamp set it explicitly; the rest get "now", which is correct for an
/// in-process store that appends as it goes. th-30a8a7.
/// </summary>
public sealed record StoredMessage(string Id, string ConversationId, MessageDirection Direction, string Text)
{
    public DateTimeOffset CreatedAt { get; init; } = DateTimeOffset.UtcNow;
}

/// <summary>
/// One row of the conversation-list / resume surface: identity, last activity, message count,
/// and the first inbound (user) message text — enough for the dispatcher to build a sidebar
/// title without a second store roundtrip. The C# analog of the Rust <c>list_conversations</c>'
/// per-conversation peek and the Go <c>ConversationSummary</c>; title formatting (markdown strip,
/// truncation, ISO timestamp) is the dispatcher's job. <c>FirstInboundText</c> is <c>null</c> when
/// the conversation has no inbound message (the title falls back to a generic name). th-d5b446.
/// </summary>
public sealed record ConversationSummary(string ConversationId, DateTimeOffset UpdatedAt, int MessageCount, string? FirstInboundText);

/// <summary>
/// A conversation's position in the sidebar order (<c>updatedAt</c> DESC, then id DESC by ordinal
/// string compare) — the keyset a <c>list_conversations</c> page resumes after (never an OFFSET).
/// On the wire it travels as an opaque cursor (<see cref="Encode"/>): unpadded base64url of
/// <c>&lt;RFC 3339 updatedAt, full precision&gt;|&lt;conversationId&gt;</c>, byte-compatible with the
/// Rust <c>ConversationKey</c>. SMOODEV-3744.
/// </summary>
public sealed record ConversationKey(DateTimeOffset UpdatedAt, string Id)
{
    private static readonly System.Text.RegularExpressions.Regex Rfc3339 = new(
        @"^(\d{4})-(\d{2})-(\d{2})[Tt](\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?([Zz]|[+-]\d{2}:\d{2})$",
        System.Text.RegularExpressions.RegexOptions.CultureInvariant);

    /// <summary>The key of <paramref name="summary"/>.</summary>
    public static ConversationKey Of(ConversationSummary summary) => new(summary.UpdatedAt, summary.ConversationId);

    /// <summary>
    /// Whether a conversation keyed (<paramref name="updatedAt"/>, <paramref name="id"/>) sorts strictly
    /// after this key — i.e. belongs to a later page. Timestamps compare at full tick precision; ids
    /// compare ordinally (for canonical lowercase UUIDs, the same order Postgres gives <c>uuid</c>).
    /// </summary>
    public bool Precedes(DateTimeOffset updatedAt, string id) =>
        updatedAt < UpdatedAt || (updatedAt == UpdatedAt && string.CompareOrdinal(id, Id) < 0);

    /// <summary>Sidebar order: newest <c>updatedAt</c> first, then id descending (ordinal).</summary>
    public static int CompareNewestFirst(ConversationSummary a, ConversationSummary b)
    {
        var byTime = b.UpdatedAt.CompareTo(a.UpdatedAt);
        return byTime != 0 ? byTime : string.CompareOrdinal(b.ConversationId, a.ConversationId);
    }

    /// <summary>The opaque wire cursor for this key.</summary>
    public string Encode()
    {
        // `FFFFFFF` drops trailing zeros (and the dot when the fraction is zero), like Rust's AutoSi.
        var ts = UpdatedAt.UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss.FFFFFFF'Z'", System.Globalization.CultureInfo.InvariantCulture);
        return Convert.ToBase64String(System.Text.Encoding.UTF8.GetBytes($"{ts}|{Id}"))
            .TrimEnd('=').Replace('+', '-').Replace('/', '_');
    }

    /// <summary>
    /// Parse a wire cursor. <c>false</c> when it isn't one a server minted: not unpadded base64url, not
    /// UTF-8, no <c>|</c>, an empty id, or a timestamp that isn't RFC 3339. Accepts any RFC 3339
    /// timestamp (offsets, <c>Z</c>, and more than 7 fractional digits — truncated to a tick).
    /// </summary>
    public static bool TryDecode(string? cursor, [System.Diagnostics.CodeAnalysis.NotNullWhen(true)] out ConversationKey? key)
    {
        key = null;
        var raw = DecodeBase64Url(cursor?.Trim());
        if (raw is null)
        {
            return false;
        }
        var pipe = raw.IndexOf('|');
        if (pipe < 0 || pipe == raw.Length - 1)
        {
            return false;
        }
        if (!TryParseRfc3339(raw[..pipe], out var updatedAt))
        {
            return false;
        }
        key = new ConversationKey(updatedAt, raw[(pipe + 1)..]);
        return true;
    }

    private static string? DecodeBase64Url(string? s)
    {
        if (string.IsNullOrEmpty(s) || s.Length % 4 == 1)
        {
            return null;
        }
        foreach (var c in s)
        {
            if (!(char.IsAsciiLetterOrDigit(c) || c == '-' || c == '_'))
            {
                return null; // standard-alphabet chars, padding and whitespace are not unpadded base64url
            }
        }
        var b64 = s.Replace('-', '+').Replace('_', '/');
        b64 = b64.PadRight(b64.Length + ((4 - (b64.Length % 4)) % 4), '=');
        try
        {
            return new System.Text.UTF8Encoding(false, throwOnInvalidBytes: true).GetString(Convert.FromBase64String(b64));
        }
        catch (Exception ex) when (ex is FormatException or ArgumentException)
        {
            return null;
        }
    }

    private static bool TryParseRfc3339(string s, out DateTimeOffset value)
    {
        value = default;
        var m = Rfc3339.Match(s);
        if (!m.Success)
        {
            return false;
        }
        int G(int i) => int.Parse(m.Groups[i].Value, System.Globalization.CultureInfo.InvariantCulture);
        try
        {
            var offset = TimeSpan.Zero;
            var zone = m.Groups[8].Value;
            if (zone is not ("Z" or "z"))
            {
                var hours = int.Parse(zone.AsSpan(1, 2), provider: System.Globalization.CultureInfo.InvariantCulture);
                var minutes = int.Parse(zone.AsSpan(4, 2), provider: System.Globalization.CultureInfo.InvariantCulture);
                if (minutes > 59)
                {
                    return false;
                }
                offset = new TimeSpan(hours, minutes, 0) * (zone[0] == '-' ? -1 : 1);
            }
            // Fraction → ticks (100 ns): pad short fractions, truncate anything past 7 digits.
            var fraction = m.Groups[7].Success ? m.Groups[7].Value : string.Empty;
            var ticks = fraction.Length == 0 ? 0 : long.Parse(fraction.PadRight(7, '0')[..7], System.Globalization.CultureInfo.InvariantCulture);
            var local = new DateTime(G(1), G(2), G(3), G(4), G(5), G(6), DateTimeKind.Unspecified).AddTicks(ticks);
            value = new DateTimeOffset(local, offset).ToUniversalTime();
            return true;
        }
        catch (ArgumentException)
        {
            return false; // out-of-range month/day/hour/offset
        }
    }
}

/// <summary>
/// One <c>list_conversations</c> page request against a store: at most <see cref="Limit"/> rows (the
/// dispatcher asks for one more than the page to learn <c>hasMore</c>), strictly after
/// <see cref="After"/>, matching <see cref="Search"/>. The C# analog of the Rust
/// <c>ConversationSummaryQuery</c>. Build it with <see cref="Create"/>, which trims the search.
/// </summary>
public sealed record ConversationPageQuery
{
    private ConversationPageQuery(int limit, ConversationKey? after, string? search)
    {
        Limit = limit;
        After = after;
        Search = search;
    }

    /// <summary>Max rows to return.</summary>
    public int Limit { get; }

    /// <summary>Return only rows strictly after this key (the previous page's last row); null = first page.</summary>
    public ConversationKey? After { get; }

    /// <summary>
    /// Case-insensitive substring a row's title source must contain; null = no filter. This server has
    /// no conversation-name concept — its sidebar title is always the first inbound message (else the
    /// default "Conversation" placeholder, which never matches) — so that message is the title source.
    /// </summary>
    public string? Search { get; }

    /// <summary>A page query; <paramref name="search"/> is trimmed and blank means no filter.</summary>
    public static ConversationPageQuery Create(int limit, ConversationKey? after, string? search)
    {
        var trimmed = search?.Trim();
        return new ConversationPageQuery(Math.Max(limit, 0), after, string.IsNullOrEmpty(trimmed) ? null : trimmed);
    }

    /// <summary>Whether <paramref name="summary"/> is past the cursor (always, with no cursor).</summary>
    public bool AdmitsPosition(ConversationSummary summary) =>
        After is null || After.Precedes(summary.UpdatedAt, summary.ConversationId);

    /// <summary>Whether a row with this first inbound text matches the search (always, with no search).</summary>
    public bool MatchesSearch(string? firstInboundText) =>
        Search is null || (firstInboundText is not null && firstInboundText.Contains(Search, StringComparison.OrdinalIgnoreCase));

    /// <summary>
    /// Apply this query to an already-SCOPED set of summaries: drop empties, keyset-filter, search,
    /// sort newest first, cap. Correct for any store whose <see cref="ISessionStore.ListConversationsAsync"/>
    /// honours the scope and is unlimited (the interface's default page implementation).
    /// </summary>
    public IReadOnlyList<ConversationSummary> Apply(IEnumerable<ConversationSummary> scoped)
    {
        var rows = scoped
            .Where(s => s.MessageCount > 0 && AdmitsPosition(s) && MatchesSearch(s.FirstInboundText))
            .ToList();
        rows.Sort(ConversationKey.CompareNewestFirst);
        return rows.Count > Limit ? rows.GetRange(0, Limit) : rows;
    }
}

/// <summary>
/// Which user's conversations a read may see. A store MUST honour this in its query, not after it —
/// filtering a page in memory after a LIMIT silently returns short or empty pages.
/// <para>
/// This is a type rather than a nullable <c>string</c> on purpose: "no filter" means "every user's
/// conversations", and that must be spelled out (<see cref="Unscoped"/>) instead of falling out of a
/// forgotten <c>null</c>. th-966fab.
/// </para>
/// </summary>
public readonly record struct ConversationScope
{
    private ConversationScope(string? userEmail, bool unscoped)
    {
        UserEmail = userEmail;
        IsUnscoped = unscoped;
    }

    /// <summary>The owning user's email when scoped; <c>null</c> for <see cref="Unscoped"/>/<see cref="None"/>.</summary>
    public string? UserEmail { get; }

    /// <summary>
    /// EVERY user's conversations. Legitimate ONLY on a server with no auth configured (single-tenant
    /// local/dev, where there is no notion of a user). Never reachable from an authenticated request.
    /// </summary>
    public bool IsUnscoped { get; }

    /// <summary>No conversations at all — an authenticated caller whose identity carries no email.</summary>
    public bool IsEmpty => !IsUnscoped && string.IsNullOrEmpty(UserEmail);

    public static ConversationScope Unscoped { get; } = new(null, unscoped: true);

    /// <summary>Nothing matches. The fail-closed scope. </summary>
    public static ConversationScope None { get; } = new(null, unscoped: false);

    public static ConversationScope ForUser(string userEmail) => new(userEmail, unscoped: false);
}

/// <summary>
/// Persistence for sessions + conversation message logs — the C# analog of the Rust
/// <c>StorageAdapter</c>'s session/conversation/message surface (and, like it, async). The
/// bundled <see cref="InMemorySessionStore"/> is the reference store; a Postgres adapter
/// (<c>SmooAI.SmoothOperator.Server.Postgres</c>) implements the same interface for durability.
/// </summary>
public interface ISessionStore
{
    Task<StoredSession> CreateSessionAsync(string agentId, string? userName, string? userEmail, CancellationToken cancellationToken = default);

    /// <summary>
    /// Mint a session bound to an existing conversation when <paramref name="conversationId"/> is
    /// non-empty AND known (reuses its message log so subsequent turns append to it and the runner
    /// replays its history); an empty or unknown <paramref name="conversationId"/> mints a fresh
    /// conversation — identical to <see cref="CreateSessionAsync"/>. The resume substrate behind
    /// <c>create_conversation_session</c>'s optional <c>conversationId</c>. th-d5b446.
    /// </summary>
    Task<StoredSession> ResumeSessionAsync(string agentId, string? userName, string? userEmail, string? conversationId, CancellationToken cancellationToken = default);

    /// <summary>
    /// A summary per conversation that has at least one message (empty conversations — every
    /// page-load currently mints one — are filtered out), in no particular order; the dispatcher
    /// sorts most-recent-first and caps. The C# analog of the Rust storage list-conversations +
    /// per-conversation peek and the Go <c>ListConversations</c>. th-d5b446.
    /// <para>
    /// SECURITY (th-966fab): <paramref name="scope"/> is REQUIRED and MUST be applied inside the
    /// query. <see cref="ConversationScope.ForUser"/> returns only conversations owned by that email;
    /// <see cref="ConversationScope.None"/> returns nothing; <see cref="ConversationScope.Unscoped"/>
    /// returns every user's conversations and is legitimate ONLY on a server with no auth configured.
    /// Ignoring the scope re-opens a cross-user data leak.
    /// </para>
    /// </summary>
    Task<IReadOnlyList<ConversationSummary>> ListConversationsAsync(ConversationScope scope, CancellationToken cancellationToken = default);

    /// <summary>
    /// One <c>list_conversations</c> page: the non-empty conversations visible to
    /// <paramref name="scope"/>, strictly after <see cref="ConversationPageQuery.After"/> in sidebar
    /// order (<c>updatedAt</c> DESC, then id DESC ordinal), matching
    /// <see cref="ConversationPageQuery.Search"/>, sorted in that order, at most
    /// <see cref="ConversationPageQuery.Limit"/> rows. SMOODEV-3744.
    /// <para>
    /// SECURITY: the scope is applied BEFORE the search and the limit, so a search only ever narrows
    /// what the caller could already list. The default implementation is correct for any store whose
    /// <see cref="ListConversationsAsync"/> honours the scope (it pages the full scoped set in memory),
    /// so a host's own store keeps working unchanged; a database store should override it to push the
    /// keyset, search and LIMIT into its query (the Postgres store does).
    /// </para>
    /// </summary>
    async Task<IReadOnlyList<ConversationSummary>> ListConversationsPageAsync(ConversationScope scope, ConversationPageQuery query, CancellationToken cancellationToken = default) =>
        query.Apply(await ListConversationsAsync(scope, cancellationToken).ConfigureAwait(false));

    /// <summary>
    /// Whether <paramref name="userEmail"/> owns <paramref name="conversationId"/> — the ownership
    /// gate behind resume and <c>get_conversation_messages</c>.
    /// <para>
    /// SECURITY (th-966fab): returns <c>false</c> for a conversation that does not exist, one owned by
    /// another user, AND one with no recorded owner (data written before per-user scoping existed).
    /// Collapsing all three into one answer is deliberate — a caller cannot tell "not yours" from
    /// "never existed", so this cannot be used to enumerate other users' conversation ids.
    /// </para>
    /// </summary>
    Task<bool> ConversationBelongsToUserAsync(string conversationId, string userEmail, CancellationToken cancellationToken = default);

    Task<StoredSession?> GetSessionAsync(string sessionId, CancellationToken cancellationToken = default);

    Task<StoredMessage> AppendMessageAsync(string conversationId, MessageDirection direction, string text, CancellationToken cancellationToken = default);

    /// <summary>The most recent <paramref name="limit"/> messages for a conversation, oldest first.</summary>
    Task<IReadOnlyList<StoredMessage>> ListMessagesAsync(string conversationId, int limit, CancellationToken cancellationToken = default);

    /// <summary>The persisted conversation-workflow step pointer for a conversation, or <c>null</c>
    /// when none has been recorded (a fresh conversation starts on the workflow's first step).
    /// Mirrors the monorepo graph state's <c>currentStepId</c>, persisted so a workflow advances
    /// across turns (and connections).</summary>
    Task<string?> GetWorkflowStepAsync(string conversationId, CancellationToken cancellationToken = default);

    /// <summary>Record the conversation's current workflow step (upsert). Called after the judge
    /// advances the pointer at the end of a turn.</summary>
    Task SetWorkflowStepAsync(string conversationId, string stepId, CancellationToken cancellationToken = default);

    /// <summary>The client render capabilities (<c>supports</c>) this conversation last declared —
    /// the gate on the entire Rich Interactions framework (a kind whose capability is listed parks the
    /// turn and emits a card; one that is missing degrades to the conversational fallback). Empty for a
    /// conversation that never declared any, which is exactly the text-only behavior.
    /// <para>
    /// CONVERSATION-scoped, not session-scoped, on purpose: a reconnect IS a resume — the client
    /// re-opens the socket and re-issues <c>create_conversation_session</c> with the same
    /// <c>conversationId</c>, which mints a NEW session on a NEW dispatcher. Capabilities that lived on
    /// the connection were therefore lost on every network blip / backgrounding / deploy unless the
    /// client re-declared them, and Rich Interactions silently stopped being offered — no error, no
    /// event, nothing on the wire to notice. th-13df6d.
    /// </para></summary>
    Task<IReadOnlyList<string>> GetClientSupportsAsync(string conversationId, CancellationToken cancellationToken = default);

    /// <summary>Record the capabilities a <c>create_conversation_session</c> frame DECLARED for this
    /// conversation (upsert). A declaration always REPLACES what was stored, including an explicit
    /// empty list — that is how a text-only channel resuming a rich conversation opts out, and the
    /// opt-out has to be durable too or the next reconnect resurrects the old set. A frame that OMITS
    /// the key is not a declaration and must not call this.</summary>
    Task SetClientSupportsAsync(string conversationId, IReadOnlyList<string> supports, CancellationToken cancellationToken = default);

    /// <summary>Whether this conversation's caller is identity-verified (the persisted
    /// <c>otpVerified</c> bit — the C# analog of the Rust session's <c>metadata.otpVerified</c>).
    /// <c>false</c> for a fresh or unknown conversation. Threaded into the <c>end_user</c> auth gate
    /// via <see cref="StoreSessionAuthenticator"/> so a verified caller's gated tools run.</summary>
    Task<bool> GetSessionAuthenticatedAsync(string conversationId, CancellationToken cancellationToken = default);

    /// <summary>Mark this conversation's caller identity-verified (or clear it). Called after a
    /// successful <c>verify_otp</c>. Upsert; no-op semantics for an unknown conversation are fine
    /// (the bit simply reads back on the next turn).</summary>
    Task SetSessionAuthenticatedAsync(string conversationId, bool verified, CancellationToken cancellationToken = default);
}

/// <summary>
/// An <see cref="ISessionAuthenticator"/> backed by the session store's persisted <c>otpVerified</c>
/// bit — the default when a host wires no authenticator of its own. Fails closed for any conversation
/// that never completed <c>verify_otp</c> (reads <c>false</c>), so an unwired server is unchanged; a
/// verified session reads <c>true</c> and its <c>end_user</c> tools run. Mirrors the Rust reference
/// threading <c>session_authenticated</c> (from session metadata) into <c>build_auth_gate</c>.
/// </summary>
public sealed class StoreSessionAuthenticator : ISessionAuthenticator
{
    private readonly ISessionStore _store;

    public StoreSessionAuthenticator(ISessionStore store) => _store = store;

    public Task<bool> IsAuthenticatedAsync(string conversationId, CancellationToken cancellationToken = default) =>
        _store.GetSessionAuthenticatedAsync(conversationId, cancellationToken);
}

/// <summary>In-process <see cref="ISessionStore"/>. The C# analog of the Rust in-memory adapter.</summary>
public sealed class InMemorySessionStore : ISessionStore
{
    private readonly object _gate = new();
    private readonly Dictionary<string, StoredSession> _sessions = new();
    private readonly Dictionary<string, List<StoredMessage>> _messages = new();
    private readonly Dictionary<string, string> _workflowSteps = new();

    // Each conversation's last-declared render capabilities (`supports`). Conversation-keyed like the
    // workflow step pointer, so a reconnect that omits the key still gets its cards. th-13df6d.
    private readonly Dictionary<string, IReadOnlyList<string>> _clientSupports = new();
    private readonly HashSet<string> _authenticated = new();

    // Each conversation's last activity (creation, then every append) — the sort key + updatedAt
    // field for ListConversations. th-d5b446.
    private readonly Dictionary<string, DateTimeOffset> _updatedAt = new();

    // Each conversation's owning user email, stamped when the conversation is minted (null when the
    // creator had no identity — no-auth servers, or an authenticated principal with no email claim).
    // The scoping key for ListConversations + the ownership gate. th-966fab.
    private readonly Dictionary<string, string?> _owner = new();

    // The clock behind last-activity times. Injectable so paging tests can pin exact (and tied)
    // updatedAt values; the system clock otherwise.
    private readonly TimeProvider _clock;

    public InMemorySessionStore()
        : this(TimeProvider.System)
    {
    }

    public InMemorySessionStore(TimeProvider clock)
    {
        _clock = clock ?? throw new ArgumentNullException(nameof(clock));
    }

    public Task<StoredSession> CreateSessionAsync(string agentId, string? userName, string? userEmail, CancellationToken cancellationToken = default) =>
        ResumeSessionAsync(agentId, userName, userEmail, null, cancellationToken);

    public Task<StoredSession> ResumeSessionAsync(string agentId, string? userName, string? userEmail, string? conversationId, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            // Resume when the caller names a known conversation (reuse its id + message log);
            // absent/unknown → a fresh conversation (byte-for-byte the old CreateSession behavior).
            var resume = !string.IsNullOrEmpty(conversationId) && _messages.ContainsKey(conversationId);
            var convId = resume ? conversationId! : Guid.NewGuid().ToString();

            var session = new StoredSession(
                SessionId: Guid.NewGuid().ToString(),
                ConversationId: convId,
                // Absent stays absent — see StoredSession.AgentId. Whitespace is absent too.
                AgentId: string.IsNullOrWhiteSpace(agentId) ? null : agentId,
                AgentName: "smooth-agent",
                UserParticipantId: Guid.NewGuid().ToString(),
                // Unlike AgentId, minting this one is CORRECT: it is a new participant row, not a
                // reference to something that has to already exist.
                AgentParticipantId: Guid.NewGuid().ToString(),
                UserEmail: string.IsNullOrEmpty(userEmail) ? null : userEmail);

            _sessions[session.SessionId] = session;
            // Only mint an empty log + creation timestamp on a fresh conversation — a resume keeps
            // its history and its last-activity time (bumped by the next append, not by re-binding).
            if (!resume)
            {
                _messages[convId] = new List<StoredMessage>();
                _updatedAt[convId] = _clock.GetUtcNow();
                _owner[convId] = session.UserEmail; // ownership is stamped once, at mint. th-966fab.
            }
            return Task.FromResult(session);
        }
    }

    public Task<IReadOnlyList<ConversationSummary>> ListConversationsAsync(ConversationScope scope, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            var summaries = new List<ConversationSummary>();
            if (scope.IsEmpty)
            {
                // An authenticated caller with no identity email owns nothing. th-966fab.
                return Task.FromResult<IReadOnlyList<ConversationSummary>>(summaries);
            }

            foreach (var (convId, list) in _messages)
            {
                if (list.Count == 0)
                {
                    continue; // drop the empty conversations every page-load mints.
                }
                if (!scope.IsUnscoped && !OwnedBy(convId, scope.UserEmail!))
                {
                    continue; // not this user's conversation. th-966fab.
                }
                var firstInbound = list.FirstOrDefault(m => m.Direction == MessageDirection.Inbound)?.Text;
                var updatedAt = _updatedAt.TryGetValue(convId, out var t) ? t : _clock.GetUtcNow();
                summaries.Add(new ConversationSummary(convId, updatedAt, list.Count, firstInbound));
            }
            IReadOnlyList<ConversationSummary> result = summaries;
            return Task.FromResult(result);
        }
    }

    public Task<bool> ConversationBelongsToUserAsync(string conversationId, string userEmail, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            return Task.FromResult(OwnedBy(conversationId, userEmail));
        }
    }

    // Unknown conversation, another user's, and one with no recorded owner are all `false` — the
    // caller cannot distinguish them, so this is not an existence oracle. Caller holds _gate.
    private bool OwnedBy(string conversationId, string userEmail) =>
        _owner.TryGetValue(conversationId, out var owner) && owner is not null && string.Equals(owner, userEmail, StringComparison.OrdinalIgnoreCase);

    public Task<StoredSession?> GetSessionAsync(string sessionId, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            return Task.FromResult(_sessions.TryGetValue(sessionId, out var session) ? session : null);
        }
    }

    public Task<StoredMessage> AppendMessageAsync(string conversationId, MessageDirection direction, string text, CancellationToken cancellationToken = default)
    {
        var message = new StoredMessage(Guid.NewGuid().ToString(), conversationId, direction, text);
        lock (_gate)
        {
            if (!_messages.TryGetValue(conversationId, out var list))
            {
                list = new List<StoredMessage>();
                _messages[conversationId] = list;
            }
            list.Add(message);
            _updatedAt[conversationId] = _clock.GetUtcNow(); // last activity → ListConversations sort key.
        }
        return Task.FromResult(message);
    }

    public Task<IReadOnlyList<StoredMessage>> ListMessagesAsync(string conversationId, int limit, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            IReadOnlyList<StoredMessage> result = _messages.TryGetValue(conversationId, out var list)
                ? list.TakeLast(limit).ToList()
                : Array.Empty<StoredMessage>();
            return Task.FromResult(result);
        }
    }

    public Task<string?> GetWorkflowStepAsync(string conversationId, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            return Task.FromResult(_workflowSteps.TryGetValue(conversationId, out var step) ? step : null);
        }
    }

    public Task SetWorkflowStepAsync(string conversationId, string stepId, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            _workflowSteps[conversationId] = stepId;
        }
        return Task.CompletedTask;
    }

    public Task<IReadOnlyList<string>> GetClientSupportsAsync(string conversationId, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            return Task.FromResult(_clientSupports.TryGetValue(conversationId, out var supports) ? supports : Array.Empty<string>());
        }
    }

    public Task SetClientSupportsAsync(string conversationId, IReadOnlyList<string> supports, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            // Copy: the caller's list must not keep mutating the stored record.
            _clientSupports[conversationId] = supports.ToArray();
        }
        return Task.CompletedTask;
    }

    public Task<bool> GetSessionAuthenticatedAsync(string conversationId, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            return Task.FromResult(_authenticated.Contains(conversationId));
        }
    }

    public Task SetSessionAuthenticatedAsync(string conversationId, bool verified, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            if (verified)
            {
                _authenticated.Add(conversationId);
            }
            else
            {
                _authenticated.Remove(conversationId);
            }
        }
        return Task.CompletedTask;
    }
}
