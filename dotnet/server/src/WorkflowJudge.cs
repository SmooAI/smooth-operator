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
/// The default LLM-backed judge. One structured yes/no/maybe call against a fast model. It reuses the
/// server's own <see cref="IChatClient"/> (same gateway + key) but always overrides the per-request
/// model to the judge model — <see cref="ServerEnv.DefaultJudgeModel"/> (<c>groq-gpt-oss-120b</c>)
/// unless <c>SMOOTH_AGENT_JUDGE_MODEL</c> / <c>SMOOTH_JUDGE_MODEL</c> or an explicit option names
/// another — so the judge never silently rides the main-turn model. Mirrors the Rust reference's
/// <c>DEFAULT_JUDGE_MODEL</c> (SMOODEV-3342). Failure-tolerant: parse/model/transport errors resolve
/// to <see cref="WorkflowVerdict.Skipped"/>.
/// </summary>
public sealed class LlmWorkflowJudge : IWorkflowJudge
{
    // ponytail: reuse the server's IChatClient rather than wiring a second client; the judge model is
    // a per-request ModelId override. Upgrade to an injected fast-client if it must differ per org.
    /// <summary>Output cap for the judge call. The verdict is a few tokens, but the default judge
    /// (<c>groq-gpt-oss-120b</c>) is a reasoning model whose reasoning counts against the cap — a
    /// small cap is spent entirely on reasoning and the reply comes back empty (gpt-oss on Groq
    /// exhausted even 200, SMOODEV-2427). 512 matches the Rust reference's <c>JUDGE_MAX_TOKENS</c>
    /// (SMOODEV-3342); the prompt still demands a one-word JSON verdict.</summary>
    public const int JudgeMaxTokens = 512;

    private readonly IChatClient _chatClient;
    private readonly string _judgeModel;

    /// <summary><paramref name="judgeModel"/> — the uniform cross-lane judge-model option. Null ⇒ the
    /// <c>SMOOTH_AGENT_JUDGE_MODEL</c> / <c>SMOOTH_JUDGE_MODEL</c> env, else
    /// <see cref="ServerEnv.DefaultJudgeModel"/>.</summary>
    public LlmWorkflowJudge(IChatClient chatClient, string? judgeModel = null)
    {
        _chatClient = chatClient ?? throw new ArgumentNullException(nameof(chatClient));
        _judgeModel = string.IsNullOrWhiteSpace(judgeModel)
            ? ServerEnv.ResolveJudgeModel(Environment.GetEnvironmentVariable)
            : judgeModel.Trim();
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
            - A brief, informal, or terse user answer that addresses the step's question satisfies it (e.g. "a four", "sure", "not really") — mark "yes"; do not hold out for elaboration or exact wording.
            - It is OK to stay on a step for multiple turns, but never require the user to re-confirm something they already said.

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
            var options = new ChatOptions { Temperature = 0f, MaxOutputTokens = JudgeMaxTokens, ModelId = _judgeModel };
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
                    var verdict = obj["verdict"].Str();
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
