using System.Text.Json;
using Npgsql;
using NpgsqlTypes;

namespace SmooAI.SmoothOperator.Server.Postgres;

/// <summary>
/// A durable <see cref="ISessionStore"/> backed by Postgres — sessions + conversation message
/// logs survive a process restart. The C# analog of the Rust <c>adapters/postgres</c> OLTP
/// surface (the <c>conversation_sessions</c> + <c>conversation_messages</c> tables, applied with
/// <c>CREATE TABLE IF NOT EXISTS</c>). Passes the same <c>ISessionStore</c> contract tests as the
/// in-memory store.
/// </summary>
public sealed class PostgresSessionStore : ISessionStore, IAsyncDisposable
{
    private const string SchemaSql = """
        CREATE TABLE IF NOT EXISTS conversation_sessions (
            session_id           TEXT PRIMARY KEY,
            conversation_id      TEXT NOT NULL,
            agent_id             TEXT NOT NULL,
            agent_name           TEXT NOT NULL,
            user_participant_id  TEXT NOT NULL,
            agent_participant_id TEXT NOT NULL,
            user_email           TEXT,
            created_at           TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE INDEX IF NOT EXISTS idx_sessions_conversation
            ON conversation_sessions (conversation_id, created_at);
        -- Idempotent for a table created before user_email existed.
        ALTER TABLE conversation_sessions ADD COLUMN IF NOT EXISTS user_email TEXT;

        -- Persisted end-user identity (OTP) verification bit per conversation. Mirrors the workflow
        -- step table's shape; the C# analog of the Rust session's metadata.otpVerified.
        CREATE TABLE IF NOT EXISTS conversation_identity_state (
            conversation_id TEXT PRIMARY KEY,
            otp_verified    BOOLEAN NOT NULL,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
        );

        CREATE TABLE IF NOT EXISTS conversation_messages (
            id              TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            direction       TEXT NOT NULL CHECK (direction IN ('inbound', 'outbound')),
            content         JSONB NOT NULL,
            seq             BIGSERIAL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE INDEX IF NOT EXISTS idx_messages_conversation_seq
            ON conversation_messages (conversation_id, seq);

        CREATE TABLE IF NOT EXISTS conversation_workflow_state (
            conversation_id TEXT PRIMARY KEY,
            step_id         TEXT NOT NULL,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        """;

    private readonly NpgsqlDataSource _dataSource;

    public PostgresSessionStore(string connectionString)
    {
        _dataSource = NpgsqlDataSource.Create(connectionString);
    }

    /// <summary>Create the store and apply the schema (idempotent).</summary>
    public static async Task<PostgresSessionStore> CreateAsync(string connectionString, CancellationToken cancellationToken = default)
    {
        var store = new PostgresSessionStore(connectionString);
        await store.InitializeAsync(cancellationToken).ConfigureAwait(false);
        return store;
    }

    public async Task InitializeAsync(CancellationToken cancellationToken = default)
    {
        await using var command = _dataSource.CreateCommand(SchemaSql);
        await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
    }

    public async Task<StoredSession> CreateSessionAsync(string agentId, string? userName, string? userEmail, CancellationToken cancellationToken = default)
    {
        var session = new StoredSession(
            SessionId: Guid.NewGuid().ToString(),
            ConversationId: Guid.NewGuid().ToString(),
            AgentId: string.IsNullOrEmpty(agentId) ? Guid.NewGuid().ToString() : agentId,
            AgentName: "smooth-agent",
            UserParticipantId: Guid.NewGuid().ToString(),
            AgentParticipantId: Guid.NewGuid().ToString(),
            UserEmail: string.IsNullOrEmpty(userEmail) ? null : userEmail);

        const string sql = """
            INSERT INTO conversation_sessions
                (session_id, conversation_id, agent_id, agent_name, user_participant_id, agent_participant_id, user_email, created_at)
            VALUES (@sid, @cid, @aid, @aname, @upid, @apid, @email, now())
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("sid", session.SessionId);
        command.Parameters.AddWithValue("cid", session.ConversationId);
        command.Parameters.AddWithValue("aid", session.AgentId);
        command.Parameters.AddWithValue("aname", session.AgentName);
        command.Parameters.AddWithValue("upid", session.UserParticipantId);
        command.Parameters.AddWithValue("apid", session.AgentParticipantId);
        command.Parameters.AddWithValue("email", (object?)session.UserEmail ?? DBNull.Value);
        await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
        return session;
    }

    public async Task<StoredSession?> GetSessionAsync(string sessionId, CancellationToken cancellationToken = default)
    {
        const string sql = """
            SELECT conversation_id, agent_id, agent_name, user_participant_id, agent_participant_id, user_email
            FROM conversation_sessions WHERE session_id = @sid
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("sid", sessionId);
        await using var reader = await command.ExecuteReaderAsync(cancellationToken).ConfigureAwait(false);
        if (!await reader.ReadAsync(cancellationToken).ConfigureAwait(false))
        {
            return null;
        }
        return new StoredSession(
            sessionId,
            reader.GetString(0),
            reader.GetString(1),
            reader.GetString(2),
            reader.GetString(3),
            reader.GetString(4),
            reader.IsDBNull(5) ? null : reader.GetString(5));
    }

    public async Task<StoredMessage> AppendMessageAsync(string conversationId, MessageDirection direction, string text, CancellationToken cancellationToken = default)
    {
        var id = Guid.NewGuid().ToString();
        var content = JsonSerializer.Serialize(new { text });

        const string sql = """
            INSERT INTO conversation_messages (id, conversation_id, direction, content, created_at)
            VALUES (@id, @cid, @dir, @content, now())
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("id", id);
        command.Parameters.AddWithValue("cid", conversationId);
        command.Parameters.AddWithValue("dir", direction == MessageDirection.Inbound ? "inbound" : "outbound");
        command.Parameters.Add(new NpgsqlParameter("content", NpgsqlDbType.Jsonb) { Value = content });
        await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
        return new StoredMessage(id, conversationId, direction, text);
    }

    public async Task<IReadOnlyList<StoredMessage>> ListMessagesAsync(string conversationId, int limit, CancellationToken cancellationToken = default)
    {
        // Most recent `limit`, returned oldest-first (the stable paging order is `seq`).
        const string sql = """
            SELECT id, direction, content->>'text' AS text
            FROM (
                SELECT id, direction, content, seq FROM conversation_messages
                WHERE conversation_id = @cid ORDER BY seq DESC LIMIT @lim
            ) sub
            ORDER BY sub.seq ASC
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("cid", conversationId);
        command.Parameters.AddWithValue("lim", limit);

        var results = new List<StoredMessage>();
        await using var reader = await command.ExecuteReaderAsync(cancellationToken).ConfigureAwait(false);
        while (await reader.ReadAsync(cancellationToken).ConfigureAwait(false))
        {
            var direction = reader.GetString(1) == "inbound" ? MessageDirection.Inbound : MessageDirection.Outbound;
            results.Add(new StoredMessage(reader.GetString(0), conversationId, direction, reader.GetString(2)));
        }
        return results;
    }

    public async Task<string?> GetWorkflowStepAsync(string conversationId, CancellationToken cancellationToken = default)
    {
        const string sql = "SELECT step_id FROM conversation_workflow_state WHERE conversation_id = @cid";
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("cid", conversationId);
        var result = await command.ExecuteScalarAsync(cancellationToken).ConfigureAwait(false);
        return result as string;
    }

    public async Task SetWorkflowStepAsync(string conversationId, string stepId, CancellationToken cancellationToken = default)
    {
        const string sql = """
            INSERT INTO conversation_workflow_state (conversation_id, step_id, updated_at)
            VALUES (@cid, @step, now())
            ON CONFLICT (conversation_id) DO UPDATE SET step_id = EXCLUDED.step_id, updated_at = now()
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("cid", conversationId);
        command.Parameters.AddWithValue("step", stepId);
        await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
    }

    public async Task<bool> GetSessionAuthenticatedAsync(string conversationId, CancellationToken cancellationToken = default)
    {
        const string sql = "SELECT otp_verified FROM conversation_identity_state WHERE conversation_id = @cid";
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("cid", conversationId);
        var result = await command.ExecuteScalarAsync(cancellationToken).ConfigureAwait(false);
        return result is bool verified && verified;
    }

    public async Task SetSessionAuthenticatedAsync(string conversationId, bool verified, CancellationToken cancellationToken = default)
    {
        const string sql = """
            INSERT INTO conversation_identity_state (conversation_id, otp_verified, updated_at)
            VALUES (@cid, @verified, now())
            ON CONFLICT (conversation_id) DO UPDATE SET otp_verified = EXCLUDED.otp_verified, updated_at = now()
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("cid", conversationId);
        command.Parameters.AddWithValue("verified", verified);
        await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
    }

    public ValueTask DisposeAsync() => _dataSource.DisposeAsync();
}
