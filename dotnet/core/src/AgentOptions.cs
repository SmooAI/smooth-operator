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
}
