using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Supplies the durable-recall handle for a turn — the C# analog of the Rust
/// <c>StorageAdapter::memory_for_access</c> seam (PR #330).
///
/// The engine already knows how to auto-recall: give <c>AgentOptions.Memory</c> a store and it
/// pulls the entries relevant to the user's message into the turn's context. What was missing on
/// this server is the way for a HOST to say <em>which</em> store — so every turn ran without
/// auto-recall regardless of what the deployment had.
///
/// The <c>access</c> argument is threaded (mirroring <c>IKnowledgeBase.ForAccess</c>) so a
/// multi-tenant backend can bind memory to the requester's org/user; a single-tenant host — Big
/// Smooth's daemon, which is the reason this seam exists — ignores it and returns its one store.
/// </summary>
public interface IMemoryProvider
{
    /// <summary>
    /// The memory to auto-recall from for a caller with this access, or <c>null</c> for none.
    /// <c>null</c> is the default for every deployment that has not opted in, and leaves the turn
    /// byte-for-byte unchanged.
    /// </summary>
    IAgentMemory? MemoryForAccess(AccessContext access);
}

/// <summary>
/// An <see cref="IMemoryProvider"/> over one unscoped store — the single-tenant case (Big Smooth's
/// daemon hands its SQLite-backed store straight through). A multi-tenant host implements the
/// interface itself and keys off <c>access</c> instead.
/// </summary>
public sealed class StaticMemoryProvider : IMemoryProvider
{
    private readonly IAgentMemory? _memory;

    /// <param name="memory">The store every caller recalls from; <c>null</c> disables auto-recall.</param>
    public StaticMemoryProvider(IAgentMemory? memory) => _memory = memory;

    /// <inheritdoc />
    public IAgentMemory? MemoryForAccess(AccessContext access) => _memory;
}
