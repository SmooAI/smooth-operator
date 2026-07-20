namespace SmooAI.SmoothOperator.Server;

/// <summary>A conversation session: the unit the protocol's create/get operate on.</summary>
public sealed record StoredSession(
    string SessionId,
    string ConversationId,
    string AgentId,
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
    private readonly HashSet<string> _authenticated = new();

    // Each conversation's last activity (creation, then every append) — the sort key + updatedAt
    // field for ListConversations. th-d5b446.
    private readonly Dictionary<string, DateTimeOffset> _updatedAt = new();

    // Each conversation's owning user email, stamped when the conversation is minted (null when the
    // creator had no identity — no-auth servers, or an authenticated principal with no email claim).
    // The scoping key for ListConversations + the ownership gate. th-966fab.
    private readonly Dictionary<string, string?> _owner = new();

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
                AgentId: string.IsNullOrEmpty(agentId) ? Guid.NewGuid().ToString() : agentId,
                AgentName: "smooth-agent",
                UserParticipantId: Guid.NewGuid().ToString(),
                AgentParticipantId: Guid.NewGuid().ToString(),
                UserEmail: string.IsNullOrEmpty(userEmail) ? null : userEmail);

            _sessions[session.SessionId] = session;
            // Only mint an empty log + creation timestamp on a fresh conversation — a resume keeps
            // its history and its last-activity time (bumped by the next append, not by re-binding).
            if (!resume)
            {
                _messages[convId] = new List<StoredMessage>();
                _updatedAt[convId] = DateTimeOffset.UtcNow;
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
                var updatedAt = _updatedAt.TryGetValue(convId, out var t) ? t : DateTimeOffset.UtcNow;
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
            _updatedAt[conversationId] = DateTimeOffset.UtcNow; // last activity → ListConversations sort key.
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
