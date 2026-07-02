namespace SmooAI.SmoothOperator.Server;

/// <summary>A conversation session: the unit the protocol's create/get operate on.</summary>
public sealed record StoredSession(
    string SessionId,
    string ConversationId,
    string AgentId,
    string AgentName,
    string UserParticipantId,
    string AgentParticipantId);

public enum MessageDirection
{
    /// <summary>From the user.</summary>
    Inbound,

    /// <summary>From the agent.</summary>
    Outbound,
}

public sealed record StoredMessage(string Id, string ConversationId, MessageDirection Direction, string Text);

/// <summary>
/// Persistence for sessions + conversation message logs — the C# analog of the Rust
/// <c>StorageAdapter</c>'s session/conversation/message surface (and, like it, async). The
/// bundled <see cref="InMemorySessionStore"/> is the reference store; a Postgres adapter
/// (<c>SmooAI.SmoothOperator.Server.Postgres</c>) implements the same interface for durability.
/// </summary>
public interface ISessionStore
{
    Task<StoredSession> CreateSessionAsync(string agentId, string? userName, string? userEmail, CancellationToken cancellationToken = default);

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
}

/// <summary>In-process <see cref="ISessionStore"/>. The C# analog of the Rust in-memory adapter.</summary>
public sealed class InMemorySessionStore : ISessionStore
{
    private readonly object _gate = new();
    private readonly Dictionary<string, StoredSession> _sessions = new();
    private readonly Dictionary<string, List<StoredMessage>> _messages = new();
    private readonly Dictionary<string, string> _workflowSteps = new();

    public Task<StoredSession> CreateSessionAsync(string agentId, string? userName, string? userEmail, CancellationToken cancellationToken = default)
    {
        var session = new StoredSession(
            SessionId: Guid.NewGuid().ToString(),
            ConversationId: Guid.NewGuid().ToString(),
            AgentId: string.IsNullOrEmpty(agentId) ? Guid.NewGuid().ToString() : agentId,
            AgentName: "smooth-agent",
            UserParticipantId: Guid.NewGuid().ToString(),
            AgentParticipantId: Guid.NewGuid().ToString());

        lock (_gate)
        {
            _sessions[session.SessionId] = session;
            _messages[session.ConversationId] = new List<StoredMessage>();
        }
        return Task.FromResult(session);
    }

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
}
