using System.Runtime.CompilerServices;
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

    /// <summary>
    /// Run the agent to completion and return the terminal <see cref="AgentRunResponse"/>.
    /// (MAF naming: <c>RunAsync</c>.)
    /// </summary>
    public async Task<AgentRunResponse> RunAsync(string message, CancellationToken cancellationToken = default)
    {
        var messages = SeedConversation(message);
        var chatOptions = BuildChatOptions();
        var usage = new UsageDetails();
        var iterations = 0;

        while (true)
        {
            iterations++;
            var response = await _chatClient.GetResponseAsync(messages, chatOptions, cancellationToken).ConfigureAwait(false);
            Accumulate(usage, response.Usage);
            messages.AddRange(response.Messages);

            var calls = ExtractToolCalls(response.Messages);
            if (calls.Count == 0 || iterations >= _options.MaxIterations)
            {
                break;
            }

            messages.Add(await ExecuteToolsAsync(calls, cancellationToken).ConfigureAwait(false));
        }

        return new AgentRunResponse(messages, usage, iterations);
    }

    /// <summary>
    /// Run the agent and stream model output as it arrives. Yields the underlying
    /// <see cref="ChatResponseUpdate"/>s (token deltas, tool-call fragments) across every
    /// loop iteration. (MAF naming: <c>RunStreamingAsync</c>.)
    /// </summary>
    public async IAsyncEnumerable<ChatResponseUpdate> RunStreamingAsync(
        string message,
        [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        var messages = SeedConversation(message);
        var chatOptions = BuildChatOptions();
        var iterations = 0;

        while (true)
        {
            iterations++;
            var updates = new List<ChatResponseUpdate>();
            await foreach (var update in _chatClient.GetStreamingResponseAsync(messages, chatOptions, cancellationToken).ConfigureAwait(false))
            {
                updates.Add(update);
                yield return update;
            }

            var response = updates.ToChatResponse();
            messages.AddRange(response.Messages);

            var calls = ExtractToolCalls(response.Messages);
            if (calls.Count == 0 || iterations >= _options.MaxIterations)
            {
                break;
            }

            messages.Add(await ExecuteToolsAsync(calls, cancellationToken).ConfigureAwait(false));
        }
    }

    private List<ChatMessage> SeedConversation(string userMessage)
    {
        var messages = new List<ChatMessage>();
        if (!string.IsNullOrEmpty(_options.Instructions))
        {
            messages.Add(new ChatMessage(ChatRole.System, _options.Instructions));
        }
        messages.Add(new ChatMessage(ChatRole.User, userMessage));
        return messages;
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
