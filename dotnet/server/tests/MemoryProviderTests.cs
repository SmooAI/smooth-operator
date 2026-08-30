using System.Text.Json.Nodes;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Durable auto-recall parity with the Rust reference (PR #330 — the
/// <c>StorageAdapter::memory_for_access</c> seam, tested in
/// <c>rust/smooth-operator-server/tests/injection_seams.rs</c>).
///
/// The engine already knew how to recall; what was missing on this server was the host's way to say
/// WHICH store, so every turn ran without auto-recall no matter what the deployment had. These tests
/// are named after their Rust counterparts so a parity gap stays visible.
///
/// The recall block's header text is deliberately NOT asserted here: the five cores currently inject
/// three different strings for it (th-ffaeae). The assertion is on the recalled CONTENT reaching the
/// model, which is the behavior the seam exists for.
/// </summary>
public class MemoryProviderTests
{
    private static async Task<string> CreateSessionAsync(FrameDispatcher dispatcher, List<JsonObject> events)
    {
        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","agentId":"11111111-1111-1111-1111-111111111111","requestId":"r1"}""", events.Add);
        var sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
        events.Clear();
        return sessionId;
    }

    /// <summary>Everything the model was sent this turn, flattened — the surface a recalled memory
    /// must show up in.</summary>
    private static string AllContentSeen(RecordingChatClient chat) =>
        string.Join("\n", chat.LastMessages.Select(m => m.Text));

    private static async Task<RecordingChatClient> RunTurnAsync(IMemoryProvider? provider, string message)
    {
        var chat = new RecordingChatClient("ok");
        var dispatcher = new FrameDispatcher(new InMemorySessionStore(), chat, memoryProvider: provider);
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        await dispatcher.DispatchAsync(
            $$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"{{message}}"}""",
            events.Add);
        await dispatcher.WaitForTurnsAsync();
        return chat;
    }

    // ── rust: no_memory_means_no_recall_injection ────────────────────────────

    /// <summary>Default: no provider ⇒ no auto-recall. Guards against the seam injecting when absent —
    /// an unopted deployment's turn must be byte-for-byte what it was before.</summary>
    [Fact]
    public async Task NoMemoryMeansNoRecallInjection()
    {
        var chat = await RunTurnAsync(null, "add shows to my watchlist");
        Assert.DoesNotContain("smoo-hub watchlist", AllContentSeen(chat), StringComparison.Ordinal);
    }

    /// <summary>A provider that returns null for this caller is the same as no provider — the seam
    /// must not fabricate a store just because one was installed.</summary>
    [Fact]
    public async Task ProviderReturningNullMeansNoRecallInjection()
    {
        var chat = await RunTurnAsync(new StaticMemoryProvider(null), "add shows to my watchlist");
        Assert.DoesNotContain("smoo-hub watchlist", AllContentSeen(chat), StringComparison.Ordinal);
    }

    // ── rust: attached_memory_is_auto_recalled_into_the_turn ─────────────────

    /// <summary>With a store attached the engine recalls the entries relevant to the user's message
    /// and injects them into the turn — the seam that lights up Big Smooth's durable auto-recall.</summary>
    [Fact]
    public async Task AttachedMemoryIsAutoRecalledIntoTheTurn()
    {
        var memory = new InMemoryAgentMemory();
        await memory.StoreAsync(new MemoryEntry("m-1", "always add shows to the smoo-hub watchlist", MemoryType.Project));

        // The message shares "add", "shows", "watchlist" with the stored entry, so the engine's
        // word-overlap recall surfaces it.
        var chat = await RunTurnAsync(new StaticMemoryProvider(memory), "add shows to my watchlist");

        Assert.Contains("smoo-hub watchlist", AllContentSeen(chat), StringComparison.Ordinal);
    }

    /// <summary>An unrelated message recalls nothing: the seam is relevance-gated by the engine, not a
    /// blanket dump of every stored memory into every turn. The message shares NO token with the entry —
    /// the bundled lexical scorer counts raw token overlap with no stopword filter, so a single shared
    /// "the" is enough to score a hit.</summary>
    [Fact]
    public async Task IrrelevantMessageRecallsNothing()
    {
        var memory = new InMemoryAgentMemory();
        await memory.StoreAsync(new MemoryEntry("m-1", "always add shows to the smoo-hub watchlist", MemoryType.Project));

        var chat = await RunTurnAsync(new StaticMemoryProvider(memory), "explain quantum entanglement");

        Assert.DoesNotContain("smoo-hub watchlist", AllContentSeen(chat), StringComparison.Ordinal);
    }

    /// <summary>The seam is access-scoped (mirroring <c>IKnowledgeBase.ForAccess</c>) so a multi-tenant
    /// host can bind memory to the requester — the argument must actually reach the provider.</summary>
    [Fact]
    public async Task ProviderSeesTheCallersAccess()
    {
        var seen = new List<AccessContext>();
        var chat = await RunTurnAsync(new RecordingMemoryProvider(seen), "hello");

        Assert.Single(seen);
    }

    private sealed class RecordingMemoryProvider : IMemoryProvider
    {
        private readonly List<AccessContext> _seen;

        public RecordingMemoryProvider(List<AccessContext> seen) => _seen = seen;

        public IAgentMemory? MemoryForAccess(AccessContext access)
        {
            _seen.Add(access);
            return null;
        }
    }
}
