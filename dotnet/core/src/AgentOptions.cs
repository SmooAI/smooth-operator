using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Core;

/// <summary>
/// Configuration for a <see cref="SmoothAgent"/> run. Mirrors the Rust engine's
/// <c>AgentConfig</c>, expressed in MEAI idioms. Later phases add memory, knowledge,
/// checkpointing, cast, HITL, and cost budgets.
/// </summary>
public sealed class AgentOptions
{
    /// <summary>Display name for the agent (used in events/tracing).</summary>
    public string Name { get; set; } = "agent";

    /// <summary>
    /// System prompt prepended to the conversation. (MAF calls this "instructions".)
    /// </summary>
    public string? Instructions { get; set; }

    /// <summary>
    /// Hard cap on agentic loop iterations (LLM calls). Stops a model that keeps
    /// requesting tools from looping forever. Mirrors the Rust engine's
    /// <c>max_iterations</c>.
    /// </summary>
    public int MaxIterations { get; set; } = 8;

    /// <summary>
    /// Tools available to the agent. Author them from ordinary C# methods with
    /// <c>AIFunctionFactory.Create(...)</c> — exactly as a Microsoft Agent Framework
    /// / Semantic Kernel dev already does.
    /// </summary>
    public IList<AITool> Tools { get; } = new List<AITool>();

    /// <summary>
    /// Soft ceiling (estimated tokens) on the conversation sent to the model. When exceeded,
    /// the <see cref="Compaction"/> strategy trims older messages before the next LLM call.
    /// Mirrors the Rust engine's <c>max_context_tokens</c>.
    /// </summary>
    public int MaxContextTokens { get; set; } = 8000;

    /// <summary>How to shrink the conversation when it exceeds <see cref="MaxContextTokens"/>.</summary>
    public CompactionStrategy Compaction { get; set; } = CompactionStrategy.SlidingWindow;

    /// <summary>
    /// Optional knowledge store. When set, the agent retrieves the top
    /// <see cref="KnowledgeTopK"/> hits for the user's message and injects them as grounding
    /// context before answering (RAG). Mirrors the Rust engine's <c>knowledge</c>.
    /// </summary>
    public IKnowledgeBase? Knowledge { get; set; }

    /// <summary>How many knowledge hits to inject per turn.</summary>
    public int KnowledgeTopK { get; set; } = 4;

    /// <summary>
    /// Optional long-/short-term memory. When set, the agent recalls the top
    /// <see cref="MemoryTopK"/> relevant memories for the user's message and injects them as
    /// context. Mirrors the Rust engine's <c>memory</c>.
    /// </summary>
    public IAgentMemory? Memory { get; set; }

    /// <summary>How many recalled memories to inject per turn.</summary>
    public int MemoryTopK { get; set; } = 4;

    /// <summary>
    /// Optional checkpoint store. When set (and a thread is in use), the agent snapshots the
    /// conversation during a run per <see cref="Checkpoint"/> so it can be resumed after a
    /// crash. Mirrors the Rust engine's <c>checkpoint_store</c>.
    /// </summary>
    public ICheckpointStore? CheckpointStore { get; set; }

    /// <summary>When to write checkpoints during a run.</summary>
    public CheckpointStrategy Checkpoint { get; set; } = CheckpointStrategy.AfterToolCall;
}
