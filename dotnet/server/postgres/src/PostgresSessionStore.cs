using System.Text.Json;
using Npgsql;
using NpgsqlTypes;

namespace SmooAI.SmoothOperator.Server.Postgres;

/// <summary>
/// A durable <see cref="ISessionStore"/> backed by Postgres — sessions + conversation message
/// logs survive a process restart. Reads and writes the SHARED schema (see
/// <see cref="PostgresSchema"/>): the same <c>conversations</c> / <c>conversation_participants</c> /
/// <c>conversation_messages</c> / <c>conversation_sessions</c> tables the Rust, Go, Python and
/// TypeScript servers use, so one database can be driven by any of them. Passes the same
/// <c>ISessionStore</c> contract tests as the in-memory store.
/// </summary>
/// <remarks>
/// The interface stays CONVERSATION-keyed (<c>GetWorkflowStepAsync(conversationId)</c> and friends)
/// while Rust/Go key the same metadata by session. That is a deliberate hold: this host's persisted
/// workflow step and OTP bit survive a resume today, and flipping to session-keyed would silently
/// require re-verification after every resume — a product decision, not a schema one. A
/// conversation-keyed write touches every session row of the conversation and a read takes the most
/// recent.
///
/// KNOWN DIVERGENCE — the data does NOT live in the same place as the other ports, only under the
/// same key names. This store writes <c>conversation_sessions.metadata</c>; the Rust/Go/Python/
/// TypeScript stores write <c>conversations.metadata_json</c> (Rust:
/// <c>rust/adapters/postgres/src/lib.rs:618</c>). So one database driven by BOTH a .NET server and
/// one of the others would not share <c>currentStepId</c>, <c>otpVerified</c>, or
/// <c>clientSupports</c> — each would read its own table and see the other's writes as absent.
/// Nothing enforces this at build time and no test pins it: you deploy ONE server implementation,
/// so the mixed-driver case is theoretical. Do not "fix" it by moving one key — that would split
/// this store's three keys across two tables, which is strictly worse. Unifying all three at once
/// is tracked as its own piece of work (th-13df6d follow-up).
/// </remarks>
public sealed class PostgresSessionStore : ISessionStore, IAsyncDisposable
{
    private readonly NpgsqlDataSource _dataSource;

    public PostgresSessionStore(string connectionString)
    {
        _dataSource = NpgsqlDataSource.Create(connectionString);
    }

    /// <summary>Create the store and apply the schema + legacy migration (both idempotent).</summary>
    public static async Task<PostgresSessionStore> CreateAsync(string connectionString, CancellationToken cancellationToken = default)
    {
        var store = new PostgresSessionStore(connectionString);
        await store.InitializeAsync(cancellationToken).ConfigureAwait(false);
        return store;
    }

    /// <summary>
    /// Create tables → widen them → index them, in that order. A database written by the OLD C#
    /// shape already has narrow tables, so <c>CREATE TABLE IF NOT EXISTS</c> leaves them as they are
    /// and the widening has to land before anything indexes the new columns.
    /// </summary>
    public async Task InitializeAsync(CancellationToken cancellationToken = default)
    {
        await ExecuteAsync(PostgresSchema.Tables, cancellationToken).ConfigureAwait(false);
        await ExecuteAsync(PostgresSchema.Migration, cancellationToken).ConfigureAwait(false);
        await ExecuteAsync(PostgresSchema.Indexes, cancellationToken).ConfigureAwait(false);
    }

    private async Task ExecuteAsync(string sql, CancellationToken cancellationToken)
    {
        await using var command = _dataSource.CreateCommand(sql);
        await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
    }

    public Task<StoredSession> CreateSessionAsync(string agentId, string? userName, string? userEmail, CancellationToken cancellationToken = default) =>
        ResumeSessionAsync(agentId, userName, userEmail, null, cancellationToken);

    public async Task<StoredSession> ResumeSessionAsync(string agentId, string? userName, string? userEmail, string? conversationId, CancellationToken cancellationToken = default)
    {
        // Resume when the caller names a known conversation (reuse its id so subsequent turns append
        // to its persisted log and the runner replays its history); absent/unknown → a fresh id
        // (byte-for-byte the old CreateSession behavior). th-d5b446.
        var resume = !string.IsNullOrEmpty(conversationId)
            && await ConversationExistsAsync(conversationId!, cancellationToken).ConfigureAwait(false);

        var email = string.IsNullOrEmpty(userEmail) ? null : userEmail;
        var session = new StoredSession(
            SessionId: Guid.NewGuid().ToString(),
            ConversationId: resume ? conversationId! : Guid.NewGuid().ToString(),
            // Absent stays absent — see StoredSession.AgentId. Whitespace is absent too.
            AgentId: string.IsNullOrWhiteSpace(agentId) ? null : agentId,
            AgentName: "smooth-agent",
            UserParticipantId: Guid.NewGuid().ToString(),
            // Unlike AgentId, minting this one is CORRECT: it is a new participant row, not a
            // reference to something that has to already exist.
            AgentParticipantId: Guid.NewGuid().ToString(),
            UserEmail: email);

        await using var connection = await _dataSource.OpenConnectionAsync(cancellationToken).ConfigureAwait(false);
        await using var transaction = await connection.BeginTransactionAsync(cancellationToken).ConfigureAwait(false);

        if (!resume)
        {
            // idempotency_key is the conversation id: the shared schema's unique index is
            // (organization_id, idempotency_key), and a freshly minted uuid is unique by construction.
            const string conversationSql = """
                INSERT INTO conversations (id, platform, name, organization_id, idempotency_key, created_at, updated_at)
                -- 'web', not 'smooth-operator': platform is the CHANNEL, and this host serves a
                -- browser WebSocket chat. The old value was the product name, which is not in the
                -- shared platform vocabulary and now fails its CHECK. Matches the Go store.
                VALUES (@cid, 'web', 'conversation', '', @cid, now(), now())
                ON CONFLICT (id) DO NOTHING
                """;
            await using (var command = new NpgsqlCommand(conversationSql, connection, transaction))
            {
                command.Parameters.AddWithValue("cid", session.ConversationId);
                await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
            }

            // The user participant is where the conversation's owner email lives — one source of
            // truth, so a resumed session reports the ORIGINAL owner (matching the Go store) rather
            // than whatever the resuming connection happened to present.
            const string participantsSql = """
                INSERT INTO conversation_participants
                    (id, conversation_id, organization_id, type, name, email, created_at, updated_at)
                VALUES (@upid, @cid, '', 'user', @uname, @email, now(), now()),
                       (@apid, @cid, '', 'ai-agent', @aname, NULL, now(), now())
                ON CONFLICT (id) DO NOTHING
                """;
            await using (var command = new NpgsqlCommand(participantsSql, connection, transaction))
            {
                command.Parameters.AddWithValue("upid", session.UserParticipantId);
                command.Parameters.AddWithValue("apid", session.AgentParticipantId);
                command.Parameters.AddWithValue("cid", session.ConversationId);
                command.Parameters.AddWithValue("uname", (object?)userName ?? "user");
                command.Parameters.AddWithValue("aname", session.AgentName);
                command.Parameters.AddWithValue("email", (object?)email ?? DBNull.Value);
                await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
            }
        }

        var metadata = JsonSerializer.Serialize(new SessionMetadata { ContactEmail = email });
        const string sessionSql = """
            INSERT INTO conversation_sessions
                (session_id, conversation_id, organization_id, agent_id, agent_name,
                 user_participant_id, agent_participant_id, thread_id, status, metadata,
                 created_at, updated_at, last_activity_at)
            VALUES (@sid, @cid, '', @aid, @aname, @upid, @apid, @cid, 'active', @metadata,
                    now(), now(), now())
            """;
        await using (var command = new NpgsqlCommand(sessionSql, connection, transaction))
        {
            command.Parameters.AddWithValue("sid", session.SessionId);
            command.Parameters.AddWithValue("cid", session.ConversationId);
            command.Parameters.AddWithValue("aid", (object?)session.AgentId ?? DBNull.Value);
            command.Parameters.AddWithValue("aname", session.AgentName);
            command.Parameters.AddWithValue("upid", session.UserParticipantId);
            command.Parameters.AddWithValue("apid", session.AgentParticipantId);
            command.Parameters.Add(new NpgsqlParameter("metadata", NpgsqlDbType.Jsonb) { Value = metadata });
            await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
        }

        await transaction.CommitAsync(cancellationToken).ConfigureAwait(false);

        // A resumed session reports the conversation's original owner, not the resuming caller's email.
        return resume
            ? session with { UserEmail = await GetOwnerEmailAsync(session.ConversationId, cancellationToken).ConfigureAwait(false) }
            : session;
    }

    /// <inheritdoc />
    public async Task<bool> ConversationBelongsToUserAsync(string conversationId, string userEmail, CancellationToken cancellationToken = default)
    {
        // Unknown conversation, another user's, and one with no recorded owner all return no row —
        // indistinguishable to the caller, so this cannot be used to probe for conversation ids.
        const string sql = """
            SELECT 1 FROM conversation_participants
            WHERE conversation_id = @cid AND type = 'user' AND lower(email) = lower(@email)
            LIMIT 1
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("cid", conversationId);
        command.Parameters.AddWithValue("email", userEmail);
        return await command.ExecuteScalarAsync(cancellationToken).ConfigureAwait(false) is not null;
    }

    private async Task<string?> GetOwnerEmailAsync(string conversationId, CancellationToken cancellationToken)
    {
        const string sql = """
            SELECT email FROM conversation_participants
            WHERE conversation_id = @cid AND type = 'user'
            ORDER BY created_at, id LIMIT 1
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("cid", conversationId);
        return await command.ExecuteScalarAsync(cancellationToken).ConfigureAwait(false) as string;
    }

    private async Task<bool> ConversationExistsAsync(string conversationId, CancellationToken cancellationToken)
    {
        const string sql = "SELECT 1 FROM conversations WHERE id = @cid LIMIT 1";
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("cid", conversationId);
        return await command.ExecuteScalarAsync(cancellationToken).ConfigureAwait(false) is not null;
    }

    public async Task<IReadOnlyList<ConversationSummary>> ListConversationsAsync(ConversationScope scope, CancellationToken cancellationToken = default)
    {
        // An authenticated caller with no identity email owns nothing — short-circuit, never query
        // unscoped. th-966fab.
        if (scope.IsEmpty)
        {
            return Array.Empty<ConversationSummary>();
        }

        // One row per conversation with at least one message: count, last-activity time (max message
        // created_at), and the FIRST inbound message text (lowest seq, direction inbound) as the title
        // source. Empty conversations are naturally excluded (no rows). Sorting + capping is the
        // dispatcher's job. The C# analog of the Rust list-conversations + per-conversation peek. th-d5b446.
        //
        // SECURITY (th-966fab): the owner filter is a WHERE inside the aggregate, NOT a post-hoc filter
        // in C# — the dispatcher applies its LIMIT to what comes back, so filtering afterwards would
        // hand back short/empty pages. A conversation with no owning user participant matches nobody.
        const string ownerFilter = """
            WHERE EXISTS (SELECT 1 FROM conversation_participants p
                           WHERE p.conversation_id = m.conversation_id
                             AND p.type = 'user'
                             AND lower(p.email) = lower(@email))
            """;
        var sql = $"""
            SELECT m.conversation_id,
                   COUNT(*)              AS message_count,
                   MAX(m.created_at)     AS updated_at,
                   (SELECT i.content->>'text' FROM conversation_messages i
                     WHERE i.conversation_id = m.conversation_id AND i.direction = 'inbound'
                     ORDER BY i.seq ASC LIMIT 1) AS first_inbound
            FROM conversation_messages m
            {(scope.IsUnscoped ? string.Empty : ownerFilter)}
            GROUP BY m.conversation_id
            """;

        await using var command = _dataSource.CreateCommand(sql);
        if (!scope.IsUnscoped)
        {
            command.Parameters.AddWithValue("email", scope.UserEmail!);
        }

        var results = new List<ConversationSummary>();
        await using var reader = await command.ExecuteReaderAsync(cancellationToken).ConfigureAwait(false);
        while (await reader.ReadAsync(cancellationToken).ConfigureAwait(false))
        {
            results.Add(new ConversationSummary(
                ConversationId: reader.GetString(0),
                UpdatedAt: reader.GetFieldValue<DateTimeOffset>(2),
                MessageCount: (int)reader.GetInt64(1),
                FirstInboundText: reader.IsDBNull(3) ? null : reader.GetString(3)));
        }
        return results;
    }

    public async Task<StoredSession?> GetSessionAsync(string sessionId, CancellationToken cancellationToken = default)
    {
        // The owner email is read from the conversation's user participant rather than duplicated onto
        // the session row, so there is one source of truth (matching the Go store).
        const string sql = """
            SELECT s.conversation_id, s.agent_id, s.agent_name, s.user_participant_id, s.agent_participant_id,
                   (SELECT p.email FROM conversation_participants p
                     WHERE p.conversation_id = s.conversation_id AND p.type = 'user'
                     ORDER BY p.created_at, p.id LIMIT 1) AS owner_email
            FROM conversation_sessions s WHERE s.session_id = @sid
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
            reader.IsDBNull(1) ? null : reader.GetString(1), // agent_id: absent is null, not a GUID
            reader.GetString(2),
            reader.GetString(3),
            reader.GetString(4),
            reader.IsDBNull(5) ? null : reader.GetString(5));
    }

    public async Task<StoredMessage> AppendMessageAsync(string conversationId, MessageDirection direction, string text, CancellationToken cancellationToken = default)
    {
        var id = Guid.NewGuid().ToString();
        // {items, text} is the shape the Rust MessageContent serializes to, so a row written here
        // reads back correctly in every other server.
        var content = JsonSerializer.Serialize(new
        {
            items = new[] { new { type = "text", text } },
            text,
        });

        const string sql = """
            INSERT INTO conversation_messages (id, organization_id, conversation_id, direction, content, created_at)
            VALUES (@id, (SELECT organization_id FROM conversations WHERE id = @cid), @cid, @dir, @content, now())
            RETURNING created_at
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("id", id);
        command.Parameters.AddWithValue("cid", conversationId);
        command.Parameters.AddWithValue("dir", direction == MessageDirection.Inbound ? "inbound" : "outbound");
        command.Parameters.Add(new NpgsqlParameter("content", NpgsqlDbType.Jsonb) { Value = content });
        // RETURNING the db-assigned now() so the caller's StoredMessage carries the SAME timestamp the
        // row was stored with (not a second, slightly-later clock read on this side). Read through a
        // typed accessor — Npgsql boxes a timestamptz as DateTime, so an unbox to DateTimeOffset
        // throws. th-30a8a7.
        await using var reader = await command.ExecuteReaderAsync(cancellationToken).ConfigureAwait(false);
        var createdAt = await reader.ReadAsync(cancellationToken).ConfigureAwait(false)
            ? reader.GetFieldValue<DateTimeOffset>(0)
            : DateTimeOffset.UtcNow;
        return new StoredMessage(id, conversationId, direction, text) { CreatedAt = createdAt };
    }

    public async Task<IReadOnlyList<StoredMessage>> ListMessagesAsync(string conversationId, int limit, CancellationToken cancellationToken = default)
    {
        // Most recent `limit`, returned oldest-first (the stable paging order is `seq`).
        const string sql = """
            SELECT id, direction, content->>'text' AS text, created_at
            FROM (
                SELECT id, direction, content, created_at, seq FROM conversation_messages
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
            results.Add(new StoredMessage(reader.GetString(0), conversationId, direction, reader.GetString(2))
            {
                CreatedAt = reader.GetFieldValue<DateTimeOffset>(3),
            });
        }
        return results;
    }

    public Task<string?> GetWorkflowStepAsync(string conversationId, CancellationToken cancellationToken = default) =>
        ReadSessionMetadataAsync(conversationId, SessionMetadata.CurrentStepIdKey, cancellationToken);

    public Task SetWorkflowStepAsync(string conversationId, string stepId, CancellationToken cancellationToken = default) =>
        MergeSessionMetadataAsync(conversationId, SessionMetadata.CurrentStepIdKey, stepId, cancellationToken);

    public async Task<IReadOnlyList<string>> GetClientSupportsAsync(string conversationId, CancellationToken cancellationToken = default)
    {
        // `->>` renders the stored JSON array as its text (`["choice_chips"]`), so parse it back.
        // Missing / unparseable ⇒ empty, i.e. the text-only behavior every kind already falls back to.
        var value = await ReadSessionMetadataAsync(conversationId, SessionMetadata.ClientSupportsKey, cancellationToken).ConfigureAwait(false);
        if (string.IsNullOrEmpty(value))
        {
            return Array.Empty<string>();
        }
        try
        {
            return JsonSerializer.Deserialize<string[]>(value) ?? Array.Empty<string>();
        }
        catch (JsonException)
        {
            return Array.Empty<string>();
        }
    }

    public Task SetClientSupportsAsync(string conversationId, IReadOnlyList<string> supports, CancellationToken cancellationToken = default) =>
        MergeSessionMetadataAsync(conversationId, SessionMetadata.ClientSupportsKey, supports.ToArray(), cancellationToken);

    public async Task<bool> GetSessionAuthenticatedAsync(string conversationId, CancellationToken cancellationToken = default)
    {
        var value = await ReadSessionMetadataAsync(conversationId, SessionMetadata.OtpVerifiedKey, cancellationToken).ConfigureAwait(false);
        return value == "true";
    }

    public Task SetSessionAuthenticatedAsync(string conversationId, bool verified, CancellationToken cancellationToken = default) =>
        MergeSessionMetadataAsync(conversationId, SessionMetadata.OtpVerifiedKey, verified, cancellationToken);

    /// <summary>
    /// The most recently created session row of the conversation that carries <paramref name="key"/>,
    /// as text (<c>-&gt;&gt;</c> renders a JSON boolean as "true"/"false"). Rows that never recorded the
    /// key are skipped rather than treated as a null answer, so an older write still reads back after a
    /// resume mints a fresh session row.
    /// </summary>
    private async Task<string?> ReadSessionMetadataAsync(string conversationId, string key, CancellationToken cancellationToken)
    {
        // jsonb_exists(metadata, @key), not `metadata ? @key`: `?` is also a parameter placeholder in
        // some drivers, so the function form keeps this unambiguous. Don't "simplify" it back.
        const string sql = """
            SELECT metadata->>@key FROM conversation_sessions
            WHERE conversation_id = @cid AND jsonb_exists(metadata, @key)
            ORDER BY created_at DESC, session_id DESC
            LIMIT 1
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("cid", conversationId);
        command.Parameters.AddWithValue("key", key);
        return await command.ExecuteScalarAsync(cancellationToken).ConfigureAwait(false) as string;
    }

    /// <summary>
    /// Merge one key into the metadata of EVERY session row of the conversation. `||` on jsonb is a
    /// shallow merge, which is all this flat object needs — it leaves the sibling keys
    /// (<c>contactEmail</c> and the other flag) alone instead of clobbering them. Writing every row
    /// rather than only the newest is what keeps this conversation-keyed surface consistent no matter
    /// which session a later read lands on. A no-op for an unknown conversation.
    /// </summary>
    private async Task MergeSessionMetadataAsync(string conversationId, string key, object value, CancellationToken cancellationToken)
    {
        var patch = JsonSerializer.Serialize(new Dictionary<string, object> { [key] = value });
        const string sql = """
            UPDATE conversation_sessions
               SET metadata = coalesce(metadata, '{}'::jsonb) || @patch::jsonb, updated_at = now()
             WHERE conversation_id = @cid
            """;
        await using var command = _dataSource.CreateCommand(sql);
        command.Parameters.AddWithValue("cid", conversationId);
        command.Parameters.AddWithValue("patch", patch);
        await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
    }

    public ValueTask DisposeAsync() => _dataSource.DisposeAsync();
}

/// <summary>
/// The JSON held in <c>conversation_sessions.metadata</c> — the per-session bits with no dedicated
/// column in the shared schema. The key names are the contract with the other four servers (the Go
/// store's <c>sessionMetadata</c> and the Rust reference server's session metadata), so they are
/// named once here rather than spelled inline at each call site.
/// </summary>
internal sealed class SessionMetadata
{
    internal const string ContactEmailKey = "contactEmail";
    internal const string OtpVerifiedKey = "otpVerified";
    internal const string CurrentStepIdKey = "currentStepId";

    /// <summary>The conversation's last-declared render capabilities (a JSON array of strings). Named
    /// to match the Rust reference's conversation-metadata key so a database driven by either server
    /// reads the same record. th-13df6d.</summary>
    internal const string ClientSupportsKey = "clientSupports";

    [System.Text.Json.Serialization.JsonPropertyName(ContactEmailKey)]
    [System.Text.Json.Serialization.JsonIgnore(Condition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull)]
    public string? ContactEmail { get; set; }
}
