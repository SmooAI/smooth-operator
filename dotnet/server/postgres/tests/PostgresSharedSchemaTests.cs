using Npgsql;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Postgres.Tests;

/// <summary>
/// This host must read and write the SHARED schema (rust/adapters/postgres/src/schema.rs, copied
/// into the Go store), not the narrower tables it used to invent. These assertions are what stops
/// it forking again: they pin the table/column shape, and they prove a database written by the OLD
/// C# shape migrates onto the shared one without losing data.
/// </summary>
public sealed class PostgresSharedSchemaTests : IClassFixture<PostgresFixture>
{
    private readonly PostgresFixture _fixture;

    public PostgresSharedSchemaTests(PostgresFixture fixture) => _fixture = fixture;

    private static async Task<bool> TableExistsAsync(string connectionString, string table)
    {
        await using var source = NpgsqlDataSource.Create(connectionString);
        await using var command = source.CreateCommand("SELECT to_regclass(@t) IS NOT NULL");
        command.Parameters.AddWithValue("t", table);
        return (bool)(await command.ExecuteScalarAsync())!;
    }

    private static async Task<bool> ColumnExistsAsync(string connectionString, string table, string column)
    {
        await using var source = NpgsqlDataSource.Create(connectionString);
        await using var command = source.CreateCommand(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = @t AND column_name = @c)");
        command.Parameters.AddWithValue("t", table);
        command.Parameters.AddWithValue("c", column);
        return (bool)(await command.ExecuteScalarAsync())!;
    }

    [SkippableTheory]
    [InlineData("conversations")]
    [InlineData("conversation_participants")]
    [InlineData("conversation_messages")]
    [InlineData("conversation_sessions")]
    public async Task Creates_TheSharedTables(string table)
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping shared-schema shape check.");
        Assert.True(await TableExistsAsync(_fixture.ConnectionString!, table), $"{table} must exist");
    }

    [SkippableTheory]
    [InlineData("conversation_identity_state")]
    [InlineData("conversation_workflow_state")]
    public async Task Drops_TheTablesThisHostInvented(string table)
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping side-table check.");
        Assert.False(await TableExistsAsync(_fixture.ConnectionString!, table), $"{table} was C#-only and must be gone");
    }

    [SkippableTheory]
    [InlineData("organization_id")]
    [InlineData("thread_id")]
    [InlineData("status")]
    [InlineData("token_count")]
    [InlineData("message_count")]
    [InlineData("metadata")]
    [InlineData("ended_at")]
    [InlineData("last_activity_at")]
    public async Task ConversationSessions_HasTheSharedColumns(string column)
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping column shape check.");
        Assert.True(await ColumnExistsAsync(_fixture.ConnectionString!, "conversation_sessions", column));
    }

    [SkippableFact]
    public async Task ConversationSessions_NoLongerCarriesUserEmail()
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping column shape check.");
        // The owner lives on the conversation's user participant — one source of truth, so a resumed
        // session reports the ORIGINAL owner (matching the Go store).
        Assert.False(await ColumnExistsAsync(_fixture.ConnectionString!, "conversation_sessions", "user_email"));
    }

    [SkippableFact]
    public async Task SessionMetadata_UsesTheKeyNamesTheOtherServersRead()
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping metadata key check.");
        var store = _fixture.Store!;

        var session = await store.CreateSessionAsync("", "Ada", "ada@example.com");
        await store.SetWorkflowStepAsync(session.ConversationId, "qualify");
        await store.SetSessionAuthenticatedAsync(session.ConversationId, true);

        // Read the raw JSON: the keys are the cross-server contract, so assert them literally rather
        // than only through the accessors that write them.
        await using var source = NpgsqlDataSource.Create(_fixture.ConnectionString!);
        await using var command = source.CreateCommand(
            "SELECT metadata->>'contactEmail', metadata->>'currentStepId', metadata->>'otpVerified' " +
            "FROM conversation_sessions WHERE session_id = @sid");
        command.Parameters.AddWithValue("sid", session.SessionId);
        await using var reader = await command.ExecuteReaderAsync();
        Assert.True(await reader.ReadAsync());
        Assert.Equal("ada@example.com", reader.GetString(0));
        Assert.Equal("qualify", reader.GetString(1));
        Assert.Equal("true", reader.GetString(2));
    }

    [SkippableFact]
    public async Task ResumedSession_ReportsTheOriginalOwner_NotTheResumingCaller()
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping owner-resolution check.");
        var store = _fixture.Store!;

        var first = await store.CreateSessionAsync("", "Ada", "owner@example.com");
        var resumed = await store.ResumeSessionAsync("", "Mallory", "mallory@example.com", first.ConversationId);

        Assert.Equal(first.ConversationId, resumed.ConversationId);
        Assert.Equal("owner@example.com", resumed.UserEmail);
        Assert.True(await store.ConversationBelongsToUserAsync(first.ConversationId, "owner@example.com"));
        Assert.False(await store.ConversationBelongsToUserAsync(first.ConversationId, "mallory@example.com"));
    }

    [SkippableFact]
    public async Task WorkflowStepAndOtpBit_SurviveAResume()
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping resume-continuity check.");
        var store = _fixture.Store!;

        var first = await store.CreateSessionAsync("", "Ada", "ada2@example.com");
        await store.SetWorkflowStepAsync(first.ConversationId, "qualify");
        await store.SetSessionAuthenticatedAsync(first.ConversationId, true);

        // Resume mints a NEW session row on the same conversation. This surface is conversation-keyed,
        // so the persisted step and OTP bit must still read back — the behavior this host had before
        // the data moved into conversation_sessions.metadata.
        var resumed = await store.ResumeSessionAsync("", "Ada", "ada2@example.com", first.ConversationId);
        Assert.NotEqual(first.SessionId, resumed.SessionId);
        Assert.Equal("qualify", await store.GetWorkflowStepAsync(resumed.ConversationId));
        Assert.True(await store.GetSessionAuthenticatedAsync(resumed.ConversationId));
    }

    /// <summary>
    /// The upgrade-in-place path: stand up a database in the OLD C# shape, put real rows in it, then
    /// open a store against it and assert every fact survived onto the shared schema. Runs in its own
    /// database on the shared container, because the fixture's database has already been migrated.
    /// </summary>
    [SkippableFact]
    public async Task LegacyCSharpDatabase_MigratesOntoTheSharedSchema()
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping legacy migration test.");

        const string legacyDb = "legacy_cs_shape";
        await using (var admin = NpgsqlDataSource.Create(_fixture.ConnectionString!))
        {
            await using var drop = admin.CreateCommand($"DROP DATABASE IF EXISTS {legacyDb}");
            await drop.ExecuteNonQueryAsync();
            await using var create = admin.CreateCommand($"CREATE DATABASE {legacyDb}");
            await create.ExecuteNonQueryAsync();
        }

        var legacyConnectionString = new NpgsqlConnectionStringBuilder(_fixture.ConnectionString!) { Database = legacyDb }.ToString();

        // The pre-convergence C# shape, verbatim: a narrow conversation_sessions carrying user_email,
        // narrow conversation_messages, and the two side tables this host invented.
        const string legacySchema = """
            CREATE TABLE conversation_sessions (
                session_id           TEXT PRIMARY KEY,
                conversation_id      TEXT NOT NULL,
                agent_id             TEXT NOT NULL,
                agent_name           TEXT NOT NULL,
                user_participant_id  TEXT NOT NULL,
                agent_participant_id TEXT NOT NULL,
                user_email           TEXT,
                created_at           TIMESTAMPTZ NOT NULL DEFAULT now()
            );
            CREATE TABLE conversation_identity_state (
                conversation_id TEXT PRIMARY KEY,
                otp_verified    BOOLEAN NOT NULL,
                updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
            );
            CREATE TABLE conversation_messages (
                id              TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                direction       TEXT NOT NULL CHECK (direction IN ('inbound', 'outbound')),
                content         JSONB NOT NULL,
                seq             BIGSERIAL,
                created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
            );
            CREATE TABLE conversation_workflow_state (
                conversation_id TEXT PRIMARY KEY,
                step_id         TEXT NOT NULL,
                updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
            );

            INSERT INTO conversation_sessions
                (session_id, conversation_id, agent_id, agent_name, user_participant_id, agent_participant_id, user_email)
            VALUES ('sess-1', 'conv-1', 'agent-1', 'smooth-agent', 'up-1', 'ap-1', 'Legacy@Example.com');
            INSERT INTO conversation_identity_state (conversation_id, otp_verified) VALUES ('conv-1', true);
            INSERT INTO conversation_workflow_state (conversation_id, step_id) VALUES ('conv-1', 'qualify');
            INSERT INTO conversation_messages (id, conversation_id, direction, content)
            VALUES ('msg-1', 'conv-1', 'inbound', '{"text":"legacy hello"}'::jsonb);
            """;
        await using (var legacy = NpgsqlDataSource.Create(legacyConnectionString))
        {
            await using var command = legacy.CreateCommand(legacySchema);
            await command.ExecuteNonQueryAsync();
        }

        // Opening the store runs the schema + migration.
        await using var store = await PostgresSessionStore.CreateAsync(legacyConnectionString);

        // The side tables are gone, and so is the user_email column.
        Assert.False(await TableExistsAsync(legacyConnectionString, "conversation_identity_state"));
        Assert.False(await TableExistsAsync(legacyConnectionString, "conversation_workflow_state"));
        Assert.False(await ColumnExistsAsync(legacyConnectionString, "conversation_sessions", "user_email"));

        // …and every fact they held reads back through the normal accessors.
        Assert.Equal("qualify", await store.GetWorkflowStepAsync("conv-1"));
        Assert.True(await store.GetSessionAuthenticatedAsync("conv-1"));

        // Ownership moved onto a user participant, and the match is still case-insensitive.
        Assert.True(await store.ConversationBelongsToUserAsync("conv-1", "legacy@example.com"));
        Assert.False(await store.ConversationBelongsToUserAsync("conv-1", "someone-else@example.com"));

        var session = await store.GetSessionAsync("sess-1");
        Assert.NotNull(session);
        Assert.Equal("conv-1", session!.ConversationId);
        Assert.Equal("Legacy@Example.com", session.UserEmail);

        // The message log survived, and the conversation is listable under its owner.
        var messages = await store.ListMessagesAsync("conv-1", 50);
        Assert.Single(messages);
        Assert.Equal("legacy hello", messages[0].Text);

        var listed = await store.ListConversationsAsync(ConversationScope.ForUser("legacy@example.com"));
        Assert.Single(listed);
        Assert.Equal("conv-1", listed[0].ConversationId);
        Assert.Equal("legacy hello", listed[0].FirstInboundText);

        // A backfilled conversations row exists, so later inserts satisfy the shared FK-shaped joins.
        Assert.True(await TableExistsAsync(legacyConnectionString, "conversations"));
        await store.AppendMessageAsync("conv-1", MessageDirection.Outbound, "after migration");
        Assert.Equal(2, (await store.ListMessagesAsync("conv-1", 50)).Count);

        // The migration closes the NOT NULLs too, so a migrated database ends up with the same
        // guarantees as a fresh one rather than silently keeping the nullable legacy columns.
        Assert.False(await IsNullableAsync(legacyConnectionString, "conversation_sessions", "metadata"));
        Assert.False(await IsNullableAsync(legacyConnectionString, "conversation_sessions", "last_activity_at"));
    }

    private static async Task<object?> ScalarAsync(string connectionString, string sql)
    {
        await using var source = NpgsqlDataSource.Create(connectionString);
        await using var command = source.CreateCommand(sql);
        return await command.ExecuteScalarAsync();
    }

    private static async Task<bool> IsNullableAsync(string connectionString, string table, string column)
    {
        var value = await ScalarAsync(connectionString,
            $"SELECT is_nullable FROM information_schema.columns WHERE table_name = '{table}' AND column_name = '{column}'");
        return (string?)value == "YES";
    }

    /// <summary>
    /// The json columns are NOT NULL DEFAULT '{}', so "absent" has ONE representation on read instead
    /// of two. This host omits them from its INSERTs, so the DEFAULT fires — it needs no coalesce, and
    /// this fails if either the NOT NULL or the omission changes.
    /// </summary>
    [SkippableFact]
    public async Task AbsentJsonReadsBackAsAnEmptyObject_NotNull()
    {
        Skip.IfNot(_fixture.Available, "Docker unavailable");
        var store = _fixture.Store!;
        var connectionString = _fixture.ConnectionString!;

        var session = await store.CreateSessionAsync("agent-json", "Ada", "ada-json@example.com");
        await store.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "hi");

        foreach (var sql in new[]
        {
            $"SELECT metadata_json  FROM conversations          WHERE id = '{session.ConversationId}'",
            $"SELECT analytics_json FROM conversations          WHERE id = '{session.ConversationId}'",
            $"SELECT metadata_json  FROM conversation_messages  WHERE conversation_id = '{session.ConversationId}'",
            $"SELECT analytics_json FROM conversation_messages  WHERE conversation_id = '{session.ConversationId}'",
            $"SELECT metadata_json  FROM conversation_participants WHERE conversation_id = '{session.ConversationId}'",
        })
        {
            Assert.Equal("{}", await ScalarAsync(connectionString, sql));
        }

        // conversation_sessions timestamps were fully nullable before; now they always carry a value.
        Assert.NotNull(await ScalarAsync(connectionString,
            $"SELECT last_activity_at FROM conversation_sessions WHERE session_id = '{session.SessionId}'"));
    }

    /// <summary>The CHECKs reject a value outside the shared vocabulary rather than storing it.</summary>
    [SkippableFact]
    public async Task PlatformAndStatusChecksRejectUnknownValues()
    {
        Skip.IfNot(_fixture.Available, "Docker unavailable");
        var connectionString = _fixture.ConnectionString!;

        // 'smooth-operator' was this host's old platform value — the CHECK is what stops it coming back.
        await Assert.ThrowsAsync<PostgresException>(() => ScalarAsync(connectionString,
            "INSERT INTO conversations (id, platform, name, organization_id, idempotency_key) " +
            "VALUES ('bad-platform', 'smooth-operator', 'x', '', 'bad-platform')"));

        await Assert.ThrowsAsync<PostgresException>(() => ScalarAsync(connectionString,
            "INSERT INTO conversation_sessions (session_id, conversation_id, agent_id, agent_name, " +
            "user_participant_id, agent_participant_id, thread_id, status) " +
            "VALUES ('bad-status', 'c', 'a', 'a', 'u', 'ag', 't', 'wat')"));
    }
}
