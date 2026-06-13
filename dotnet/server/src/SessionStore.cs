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
/// <c>StorageAdapter</c>'s session/conversation/message surface. The bundled
/// <see cref="InMemorySessionStore"/> is the reference store; a Postgres/Dynamo adapter
/// implements the same interface in a later phase.
/// </summary>
public interface ISessionStore
{
    StoredSession CreateSession(string agentId, string? userName, string? userEmail);

    StoredSession? GetSession(string sessionId);

    StoredMessage AppendMessage(string conversationId, MessageDirection direction, string text);

    IReadOnlyList<StoredMessage> ListMessages(string conversationId, int limit);
}

/// <summary>In-process <see cref="ISessionStore"/>. The C# analog of the Rust in-memory adapter.</summary>
public sealed class InMemorySessionStore : ISessionStore
{
    private readonly object _gate = new();
    private readonly Dictionary<string, StoredSession> _sessions = new();
    private readonly Dictionary<string, List<StoredMessage>> _messages = new();

    public StoredSession CreateSession(string agentId, string? userName, string? userEmail)
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
        return session;
    }

    public StoredSession? GetSession(string sessionId)
    {
        lock (_gate)
        {
            return _sessions.TryGetValue(sessionId, out var session) ? session : null;
        }
    }

    public StoredMessage AppendMessage(string conversationId, MessageDirection direction, string text)
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
        return message;
    }

    public IReadOnlyList<StoredMessage> ListMessages(string conversationId, int limit)
    {
        lock (_gate)
        {
            if (!_messages.TryGetValue(conversationId, out var list))
            {
                return Array.Empty<StoredMessage>();
            }
            return list.TakeLast(limit).ToList();
        }
    }
}
