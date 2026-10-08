using SmooAI.SmoothOperator.Server;
using Testcontainers.PostgreSql;

namespace SmooAI.SmoothOperator.Server.Postgres.Tests;

/// <summary>
/// Spins up a real Postgres in a container for the class. If Docker is unavailable, it degrades
/// to "unavailable" and the tests skip cleanly (never fail) — matching the repo's gated-test rule.
/// </summary>
public sealed class PostgresFixture : IAsyncLifetime
{
    private PostgreSqlContainer? _container;

    public PostgresSessionStore? Store { get; private set; }

    public PostgresKnowledgeBase? Knowledge { get; private set; }

    public PostgresAclKnowledgeStore? AclKnowledge { get; private set; }

    public PostgresCheckpointStore? CheckpointStore { get; private set; }

    public string? ConnectionString { get; private set; }

    public bool Available => Store is not null && ConnectionString is not null;

    public async Task InitializeAsync()
    {
        try
        {
            // The pgvector image is a superset of postgres — serves both the OLTP session store
            // and the vector-searched knowledge adapters from one container.
            _container = new PostgreSqlBuilder().WithImage("pgvector/pgvector:pg16").Build();
            await _container.StartAsync();
            ConnectionString = _container.GetConnectionString();
            Store = await PostgresSessionStore.CreateAsync(ConnectionString);
            Knowledge = await PostgresKnowledgeBase.CreateAsync(ConnectionString, new DeterministicEmbedder(256));
            AclKnowledge = await PostgresAclKnowledgeStore.CreateAsync(ConnectionString, new DeterministicEmbedder(256));
            CheckpointStore = await PostgresCheckpointStore.CreateAsync(ConnectionString);
        }
        catch
        {
            // Docker not reachable — leave Available == false so tests skip.
            Store = null;
            Knowledge = null;
            AclKnowledge = null;
            CheckpointStore = null;
            ConnectionString = null;
        }
    }

    public async Task DisposeAsync()
    {
        if (CheckpointStore is not null)
        {
            await CheckpointStore.DisposeAsync();
        }
        if (AclKnowledge is not null)
        {
            await AclKnowledge.DisposeAsync();
        }
        if (Knowledge is not null)
        {
            await Knowledge.DisposeAsync();
        }
        if (Store is not null)
        {
            await Store.DisposeAsync();
        }
        if (_container is not null)
        {
            await _container.DisposeAsync();
        }
    }
}

/// <summary>The shared contract, against the Postgres adapter (gated on Docker).</summary>
public sealed class PostgresSessionStoreContractTests : SessionStoreContractTests, IClassFixture<PostgresFixture>
{
    private readonly PostgresFixture _fixture;

    public PostgresSessionStoreContractTests(PostgresFixture fixture) => _fixture = fixture;

    protected override Task<ISessionStore> CreateStoreAsync()
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping Postgres adapter contract.");
        return Task.FromResult<ISessionStore>(_fixture.Store!);
    }

    /// <summary>
    /// A timestamp tie group straddling a page boundary, at Postgres' microsecond precision: the keyset
    /// comparison must break ties by conversation id (byte order, not the DB collation) so every row of
    /// the group comes back exactly once. Forces the tie by rewriting <c>created_at</c> directly.
    /// </summary>
    [SkippableFact]
    public async Task ListConversationsPage_TieGroupStraddlingAPageBoundary_PagesOnceEach()
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping Postgres paging test.");
        var store = _fixture.Store!;
        var me = $"tie-{Guid.NewGuid():N}@example.com";
        var ids = await SeedOwnedAsync(store, me, 5);

        await using (var conn = new Npgsql.NpgsqlConnection(_fixture.ConnectionString))
        {
            await conn.OpenAsync();
            await using var cmd = conn.CreateCommand();
            // Three tied at one µs-precision instant; the other two strictly newer / older.
            cmd.CommandText = """
                UPDATE conversation_messages SET created_at = CASE conversation_id
                    WHEN @a THEN TIMESTAMPTZ '2026-10-08 14:30:00.123456+00'
                    WHEN @e THEN TIMESTAMPTZ '2026-10-08 14:29:00.000001+00'
                    ELSE TIMESTAMPTZ '2026-10-08 14:29:30.654321+00' END
                WHERE conversation_id = ANY(@ids)
                """;
            cmd.Parameters.AddWithValue("a", ids[0]);
            cmd.Parameters.AddWithValue("e", ids[4]);
            cmd.Parameters.AddWithValue("ids", ids.ToArray());
            await cmd.ExecuteNonQueryAsync();
        }

        var scope = ConversationScope.ForUser(me);
        var expected = NewestFirst(await store.ListConversationsAsync(scope));
        Assert.Equal(ids[0], expected[0]);
        Assert.Equal(ids[4], expected[4]);
        Assert.Equal(ids.Skip(1).Take(3).OrderByDescending(x => x, StringComparer.Ordinal), expected.Skip(1).Take(3));

        var pages = await AllPagesAsync(store, scope, 2); // [a, t1] [t2, t3] [e] — the tie group straddles
        Assert.Equal(new[] { 2, 2, 1 }, pages.Select(p => p.Count));
        Assert.Equal(expected, pages.SelectMany(p => p));

        // The cursor round-trips the stored µs exactly (no drift that would skip or repeat a tied row).
        var first = await store.ListConversationsPageAsync(scope, ConversationPageQuery.Create(1, null, null));
        Assert.True(ConversationKey.TryDecode(ConversationKey.Of(first[0]).Encode(), out var key));
        Assert.Equal(first[0].UpdatedAt, key!.UpdatedAt);
    }

    [SkippableFact]
    public async Task Session_And_History_SurviveAcrossStoreInstances()
    {
        Skip.IfNot(_fixture.Available, "Docker/Postgres unavailable — skipping durability test.");

        // "Process 1" writes…
        await using var first = await PostgresSessionStore.CreateAsync(_fixture.ConnectionString!);
        var session = await first.CreateSessionAsync("", "Bob", null);
        await first.AppendMessageAsync(session.ConversationId, MessageDirection.Inbound, "persist me");

        // …a fresh store instance ("restart") still sees the durable session + history.
        await using var second = await PostgresSessionStore.CreateAsync(_fixture.ConnectionString!);
        var fetched = await second.GetSessionAsync(session.SessionId);
        Assert.NotNull(fetched);
        Assert.Equal(session.ConversationId, fetched!.ConversationId);

        var messages = await second.ListMessagesAsync(session.ConversationId, 50);
        Assert.Single(messages);
        Assert.Equal("persist me", messages[0].Text);
    }
}
