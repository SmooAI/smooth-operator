using System.Runtime.CompilerServices;
using System.Text;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Core;

/// <summary>
/// The smooth-operator agent engine, native C#. It drives the agentic loop over any
/// <see cref="IChatClient"/>: call the model, execute any requested tools, feed the
/// results back, and repeat until the model answers without tool calls (or the
/// iteration cap is hit). This is the in-process sibling of the Rust
/// <c>smooai-smooth-operator-core</c> <c>Agent</c>; behavioral parity is enforced by
/// the shared conformance fixtures + eval scenarios, not identical type shapes.
///
/// We own the loop deliberately (rather than delegating to MEAI's
/// <c>FunctionInvokingChatClient</c>) so later phases can layer in checkpointing,
/// HITL pause/resume, knowledge/memory injection, cost budgets, and cast/subagents
/// with smooth-operator's exact semantics.
///
/// Multi-turn conversations carry through a <see cref="SmoothAgentThread"/>; the
/// conversation is trimmed to <see cref="AgentOptions.MaxContextTokens"/> before each
/// LLM call via <see cref="AgentOptions.Compaction"/>.
/// </summary>
public sealed class SmoothAgent
{
    private readonly IChatClient _chatClient;
    private readonly AgentOptions _options;
    private readonly Dictionary<string, AIFunction> _functions;

    public SmoothAgent(IChatClient chatClient, AgentOptions options)
    {
        _chatClient = chatClient ?? throw new ArgumentNullException(nameof(chatClient));
        _options = options ?? throw new ArgumentNullException(nameof(options));
        _functions = options.Tools.OfType<AIFunction>().ToDictionary(f => f.Name, StringComparer.Ordinal);
    }

    /// <summary>Start a fresh conversation thread for multi-turn use. (MAF: <c>GetNewThread</c>.)</summary>
    public SmoothAgentThread GetNewThread() => new();

    /// <summary>Run a single stateless turn (no carried history).</summary>
    public Task<AgentRunResponse> RunAsync(string message, CancellationToken cancellationToken = default) =>
        RunAsync(message, null, cancellationToken);

    /// <summary>
    /// Run a turn within <paramref name="thread"/> (or stateless if null). The thread's prior
    /// messages are prepended; the new user/assistant/tool messages from this turn are appended
    /// back to it. (MAF naming: <c>RunAsync</c>.)
    /// </summary>
    public async Task<AgentRunResponse> RunAsync(string message, SmoothAgentThread? thread, CancellationToken cancellationToken = default)
    {
        var working = await SeedConversationAsync(message, thread, cancellationToken).ConfigureAwait(false);
        var newThisTurn = new List<ChatMessage> { working[^1] }; // the live user message
        var chatOptions = BuildChatOptions();
        var usage = new UsageDetails();
        var iterations = 0;

        while (true)
        {
            iterations++;
            Compactor.Compact(working, _options.Compaction, _options.MaxContextTokens);

            var response = await _chatClient.GetResponseAsync(working, chatOptions, cancellationToken).ConfigureAwait(false);
            Accumulate(usage, response.Usage);
            working.AddRange(response.Messages);
            newThisTurn.AddRange(response.Messages);
            await MaybeCheckpointAsync(thread, newThisTurn, iterations, CheckpointStrategy.AfterEachIteration, cancellationToken).ConfigureAwait(false);

            var calls = ExtractToolCalls(response.Messages);
            if (calls.Count == 0 || iterations >= _options.MaxIterations)
            {
                break;
            }

            var toolMessage = await ExecuteToolsAsync(calls, cancellationToken).ConfigureAwait(false);
            working.Add(toolMessage);
            newThisTurn.Add(toolMessage);
            await MaybeCheckpointAsync(thread, newThisTurn, iterations, CheckpointStrategy.AfterToolCall, cancellationToken).ConfigureAwait(false);
        }

        thread?.AddRange(newThisTurn);
        return new AgentRunResponse(newThisTurn, usage, iterations);
    }

    /// <summary>Stream a single stateless turn.</summary>
    public IAsyncEnumerable<ChatResponseUpdate> RunStreamingAsync(string message, CancellationToken cancellationToken = default) =>
        RunStreamingAsync(message, null, cancellationToken);

    /// <summary>
    /// Stream a turn within <paramref name="thread"/> (or stateless if null), yielding the
    /// model's <see cref="ChatResponseUpdate"/>s across every loop iteration. New messages are
    /// appended back to the thread when the turn completes. (MAF naming: <c>RunStreamingAsync</c>.)
    /// </summary>
    public async IAsyncEnumerable<ChatResponseUpdate> RunStreamingAsync(
        string message,
        SmoothAgentThread? thread,
        [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        var working = await SeedConversationAsync(message, thread, cancellationToken).ConfigureAwait(false);
        var newThisTurn = new List<ChatMessage> { working[^1] };
        var chatOptions = BuildChatOptions();
        var iterations = 0;

        while (true)
        {
            iterations++;
            Compactor.Compact(working, _options.Compaction, _options.MaxContextTokens);

            var updates = new List<ChatResponseUpdate>();
            await foreach (var update in _chatClient.GetStreamingResponseAsync(working, chatOptions, cancellationToken).ConfigureAwait(false))
            {
                updates.Add(update);
                yield return update;
            }

            var response = updates.ToChatResponse();
            working.AddRange(response.Messages);
            newThisTurn.AddRange(response.Messages);
            await MaybeCheckpointAsync(thread, newThisTurn, iterations, CheckpointStrategy.AfterEachIteration, cancellationToken).ConfigureAwait(false);

            var calls = ExtractToolCalls(response.Messages);
            if (calls.Count == 0 || iterations >= _options.MaxIterations)
            {
                break;
            }

            var toolMessage = await ExecuteToolsAsync(calls, cancellationToken).ConfigureAwait(false);
            working.Add(toolMessage);
            newThisTurn.Add(toolMessage);
            await MaybeCheckpointAsync(thread, newThisTurn, iterations, CheckpointStrategy.AfterToolCall, cancellationToken).ConfigureAwait(false);
        }

        thread?.AddRange(newThisTurn);
    }

    /// <summary>
    /// Reconstruct a thread from its latest checkpoint (or a fresh one with that id if there's
    /// no checkpoint / no store). The C# analog of the Rust engine's <c>resume_or_new</c>.
    /// </summary>
    public async Task<SmoothAgentThread> ResumeThreadAsync(string threadId, CancellationToken cancellationToken = default)
    {
        var thread = new SmoothAgentThread(threadId);
        if (_options.CheckpointStore is not null)
        {
            var checkpoint = await _options.CheckpointStore.LoadLatestAsync(threadId, cancellationToken).ConfigureAwait(false);
            if (checkpoint is not null)
            {
                thread.AddRange(checkpoint.Messages);
            }
        }
        return thread;
    }

    private async Task MaybeCheckpointAsync(
        SmoothAgentThread? thread,
        IReadOnlyList<ChatMessage> newThisTurn,
        int iteration,
        CheckpointStrategy trigger,
        CancellationToken cancellationToken)
    {
        if (thread is null || _options.CheckpointStore is null || _options.Checkpoint != trigger)
        {
            return;
        }

        // Snapshot the durable conversation up to this point (prior thread history + this turn
        // so far). A copy, so later compaction of the working list can't corrupt it.
        var snapshot = thread.Messages.Concat(newThisTurn).ToList();
        var checkpoint = new Checkpoint(Guid.NewGuid().ToString("n"), thread.Id, snapshot, iteration, DateTimeOffset.UtcNow);
        await _options.CheckpointStore.SaveAsync(checkpoint, cancellationToken).ConfigureAwait(false);
    }

    private async Task<List<ChatMessage>> SeedConversationAsync(string userMessage, SmoothAgentThread? thread, CancellationToken cancellationToken)
    {
        var messages = new List<ChatMessage>();
        if (!string.IsNullOrEmpty(_options.Instructions))
        {
            messages.Add(new ChatMessage(ChatRole.System, _options.Instructions));
        }
        if (thread is not null)
        {
            messages.AddRange(thread.Messages);
        }

        // Retrieve knowledge + memory for this turn and inject it as grounding context,
        // placed right before the live user message. Ephemeral — regenerated each turn,
        // never persisted into the thread.
        var context = await BuildRetrievedContextAsync(userMessage, cancellationToken).ConfigureAwait(false);
        if (context is not null)
        {
            messages.Add(context);
        }

        messages.Add(new ChatMessage(ChatRole.User, userMessage));
        return messages;
    }

    private async Task<ChatMessage?> BuildRetrievedContextAsync(string query, CancellationToken cancellationToken)
    {
        if (_options.Knowledge is null && _options.Memory is null)
        {
            return null;
        }

        var builder = new StringBuilder();

        if (_options.Knowledge is not null)
        {
            var hits = await _options.Knowledge.QueryAsync(query, _options.KnowledgeTopK, cancellationToken).ConfigureAwait(false);
            if (hits.Count > 0)
            {
                builder.AppendLine("Relevant knowledge (ground your answer in this; cite the source):");
                foreach (var hit in hits)
                {
                    builder.AppendLine($"- [{hit.Source}] {hit.Chunk}");
                }
            }
        }

        if (_options.Memory is not null)
        {
            var memories = await _options.Memory.RecallAsync(query, _options.MemoryTopK, cancellationToken).ConfigureAwait(false);
            if (memories.Count > 0)
            {
                if (builder.Length > 0)
                {
                    builder.AppendLine();
                }
                builder.AppendLine("Relevant memory:");
                foreach (var memory in memories)
                {
                    builder.AppendLine($"- {memory.Content}");
                }
            }
        }

        return builder.Length > 0 ? new ChatMessage(ChatRole.System, builder.ToString()) : null;
    }

    private ChatOptions? BuildChatOptions() =>
        _options.Tools.Count > 0 ? new ChatOptions { Tools = _options.Tools.ToList() } : null;

    private static List<FunctionCallContent> ExtractToolCalls(IEnumerable<ChatMessage> messages) =>
        messages.SelectMany(m => m.Contents).OfType<FunctionCallContent>().ToList();

    private async Task<ChatMessage> ExecuteToolsAsync(IReadOnlyList<FunctionCallContent> calls, CancellationToken cancellationToken)
    {
        var results = new List<AIContent>(calls.Count);
        foreach (var call in calls)
        {
            results.Add(await InvokeToolAsync(call, cancellationToken).ConfigureAwait(false));
        }
        return new ChatMessage(ChatRole.Tool, results);
    }

    private async Task<FunctionResultContent> InvokeToolAsync(FunctionCallContent call, CancellationToken cancellationToken)
    {
        if (!_functions.TryGetValue(call.Name, out var function))
        {
            return new FunctionResultContent(call.CallId, $"Error: unknown tool '{call.Name}'");
        }

        try
        {
            var arguments = new AIFunctionArguments(call.Arguments);
            var result = await function.InvokeAsync(arguments, cancellationToken).ConfigureAwait(false);
            return new FunctionResultContent(call.CallId, result);
        }
        catch (Exception ex)
        {
            // A failing tool is fed back to the model as an error result, not thrown —
            // the model can recover or apologize. Mirrors the Rust ToolResult.is_error path.
            return new FunctionResultContent(call.CallId, $"Error: {ex.Message}");
        }
    }

    private static void Accumulate(UsageDetails total, UsageDetails? add)
    {
        if (add is null)
        {
            return;
        }
        total.InputTokenCount = (total.InputTokenCount ?? 0) + (add.InputTokenCount ?? 0);
        total.OutputTokenCount = (total.OutputTokenCount ?? 0) + (add.OutputTokenCount ?? 0);
        total.TotalTokenCount = (total.TotalTokenCount ?? 0) + (add.TotalTokenCount ?? 0);
    }
}
