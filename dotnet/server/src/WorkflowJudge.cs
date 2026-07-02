using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Server;

/// <summary>Post-turn judge verdict. <c>Skipped</c> means "nothing evaluated" (no workflow, blocked
/// turn, empty reply, or a judge failure) — the workflow always stays on the current step.</summary>
public enum WorkflowVerdict
{
    Yes,
    No,
    Maybe,
    Skipped,
}

/// <summary>
/// Decides, after a turn, whether the current workflow step's criteria were satisfied — the C#
/// analog of the monorepo's <c>workflow-judge</c> node. A single cheap-model call; MUST be
/// failure-tolerant (any error → <see cref="WorkflowVerdict.Skipped"/> so the conversation never
/// freezes or jumps).
/// </summary>
public interface IWorkflowJudge
{
    Task<WorkflowVerdict> JudgeAsync(ConversationWorkflow workflow, ConversationWorkflowStep step, string userMessage, string agentReply, CancellationToken cancellationToken = default);
}

/// <summary>
/// The default LLM-backed judge. One structured yes/no/maybe call against a cheap model. Mirrors the
/// monorepo's Haiku-class fast-tier judge: the server's own <see cref="IChatClient"/> is already the
/// cheap default model (SMOOTH_MODEL defaults to <c>claude-haiku-4-5</c>), so it's reused unless a
/// distinct <c>SMOOTH_JUDGE_MODEL</c> is set. Failure-tolerant: parse/model/transport errors resolve
/// to <see cref="WorkflowVerdict.Skipped"/>.
/// </summary>
public sealed class LlmWorkflowJudge : IWorkflowJudge
{
    // ponytail: reuse the server's IChatClient (already the cheap default model) rather than wiring a
    // second client. Override the per-request model via SMOOTH_JUDGE_MODEL if the deploy wants a
    // distinct cheaper slot; upgrade to an injected fast-client if the two models must differ per org.
    private const int JudgeMaxTokens = 200;

    private readonly IChatClient _chatClient;
    private readonly string? _modelOverride;

    public LlmWorkflowJudge(IChatClient chatClient, string? modelOverride = null)
    {
        _chatClient = chatClient ?? throw new ArgumentNullException(nameof(chatClient));
        _modelOverride = string.IsNullOrWhiteSpace(modelOverride) ? Environment.GetEnvironmentVariable("SMOOTH_JUDGE_MODEL") : modelOverride;
    }

    public async Task<WorkflowVerdict> JudgeAsync(ConversationWorkflow workflow, ConversationWorkflowStep step, string userMessage, string agentReply, CancellationToken cancellationToken = default)
    {
        // Nothing to judge → stay put (mirrors the TS "no reply" short-circuit).
        if (string.IsNullOrWhiteSpace(agentReply))
        {
            return WorkflowVerdict.Skipped;
        }

        const string system = """
            You are a conversation-workflow judge. Given the CURRENT STEP's intent + criteria and the most recent agent reply, decide whether the step was satisfied this turn.

            Rules:
            - "yes" -> the criteria are clearly satisfied on the basis of this turn.
            - "no" -> not satisfied, or the agent moved away from the step.
            - "maybe" -> partial/ambiguous progress. The workflow will stay on the current step and try again next turn.
            - Only mark "yes" when the criteria are objectively met. It is OK to stay on a step for multiple turns.

            Reply with ONLY a JSON object: {"verdict":"yes|no|maybe","reason":"one sentence"}.
            """;

        var human = $"""
            GOAL: {workflow.Goal}

            CURRENT STEP ({step.Id}):
              intent: {step.Intent}
              criteria: {step.Criteria}

            LAST USER MESSAGE:
            {(string.IsNullOrWhiteSpace(userMessage) ? "(none)" : userMessage)}

            AGENT REPLY:
            {agentReply}

            Return a JSON object with a verdict and a one-sentence reason.
            """;

        try
        {
            var options = new ChatOptions { Temperature = 0f, MaxOutputTokens = JudgeMaxTokens };
            if (!string.IsNullOrWhiteSpace(_modelOverride))
            {
                options.ModelId = _modelOverride;
            }
            var response = await _chatClient.GetResponseAsync(
                new[] { new ChatMessage(ChatRole.System, system), new ChatMessage(ChatRole.User, human) },
                options,
                cancellationToken).ConfigureAwait(false);

            return ParseVerdict(response.Text);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            // Never freeze the conversation on a judge failure — stay on the current step.
            return WorkflowVerdict.Skipped;
        }
    }

    /// <summary>Extract the verdict from the model's reply. Tolerant: pulls the first JSON object out
    /// of the text (models often fence or preamble it); an unparseable/absent verdict → Skipped.</summary>
    public static WorkflowVerdict ParseVerdict(string? text)
    {
        if (string.IsNullOrWhiteSpace(text))
        {
            return WorkflowVerdict.Skipped;
        }

        var start = text.IndexOf('{');
        var end = text.LastIndexOf('}');
        if (start >= 0 && end > start)
        {
            try
            {
                if (JsonNode.Parse(text[start..(end + 1)]) is JsonObject obj)
                {
                    var verdict = obj["verdict"]?.GetValue<string>();
                    return verdict?.Trim().ToLowerInvariant() switch
                    {
                        "yes" => WorkflowVerdict.Yes,
                        "no" => WorkflowVerdict.No,
                        "maybe" => WorkflowVerdict.Maybe,
                        _ => WorkflowVerdict.Skipped,
                    };
                }
            }
            catch (Exception ex) when (ex is JsonException or FormatException or InvalidOperationException)
            {
                return WorkflowVerdict.Skipped;
            }
        }
        return WorkflowVerdict.Skipped;
    }
}
