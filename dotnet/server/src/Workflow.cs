namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Pure (no LLM / no I/O) helpers for resolving and rendering a structured conversation workflow —
/// the C# analog of the monorepo's <c>general-agent/workflow.ts</c> (<c>resolveCurrentStep</c>,
/// <c>nextStep</c>, <c>renderWorkflowPromptSection</c>). Kept free of dependencies so the stepping
/// logic and prompt rendering are trivially unit-testable.
/// </summary>
public static class Workflows
{
    /// <summary>
    /// Resolve the current step for a workflow + pointer:
    /// the step whose id matches <paramref name="currentStepId"/>, else the first step (fresh start
    /// or unknown pointer), else <c>null</c> when the workflow has no steps.
    /// </summary>
    public static ConversationWorkflowStep? ResolveCurrentStep(ConversationWorkflow? workflow, string? currentStepId)
    {
        if (workflow is null || workflow.Steps.Count == 0)
        {
            return null;
        }
        if (!string.IsNullOrEmpty(currentStepId))
        {
            var found = workflow.Steps.FirstOrDefault(s => s.Id == currentStepId);
            if (found is not null)
            {
                return found;
            }
        }
        return workflow.Steps[0];
    }

    /// <summary>
    /// The step to advance to once <paramref name="current"/> is satisfied. Preference order:
    /// (1) explicit <see cref="ConversationWorkflowStep.Next"/> if it resolves to a known step,
    /// (2) the element immediately after <paramref name="current"/> in <see cref="ConversationWorkflow.Steps"/>,
    /// (3) <c>null</c> — workflow complete (terminal step).
    /// </summary>
    public static ConversationWorkflowStep? NextStep(ConversationWorkflow workflow, ConversationWorkflowStep current)
    {
        if (!string.IsNullOrEmpty(current.Next))
        {
            var explicitNext = workflow.Steps.FirstOrDefault(s => s.Id == current.Next);
            if (explicitNext is not null)
            {
                return explicitNext;
            }
        }
        var idx = -1;
        for (var i = 0; i < workflow.Steps.Count; i++)
        {
            if (workflow.Steps[i].Id == current.Id)
            {
                idx = i;
                break;
            }
        }
        if (idx == -1)
        {
            return null;
        }
        return idx + 1 < workflow.Steps.Count ? workflow.Steps[idx + 1] : null;
    }

    /// <summary>
    /// Render the current step as a <c>&lt;ConversationWorkflow&gt;</c> block for the system prompt,
    /// mirroring <c>renderWorkflowPromptSection</c>. Returns the empty string when no workflow / step
    /// applies, so a caller can interpolate it unconditionally.
    /// </summary>
    public static string RenderPromptSection(ConversationWorkflow? workflow, string? currentStepId)
    {
        var step = ResolveCurrentStep(workflow, currentStepId);
        if (workflow is null || step is null)
        {
            return string.Empty;
        }
        var idx = -1;
        for (var i = 0; i < workflow.Steps.Count; i++)
        {
            if (workflow.Steps[i].Id == step.Id)
            {
                idx = i;
                break;
            }
        }
        var stepNumber = idx >= 0 ? idx + 1 : 1;
        var total = workflow.Steps.Count;
        return $"""
            <ConversationWorkflow>
            GOAL: {workflow.Goal}

            CURRENT STEP ({stepNumber}/{total}): {step.Id}
            INTENT: {step.Intent}
            CRITERIA: {step.Criteria}

            Focus this turn on the CURRENT STEP. Pursue the INTENT and aim to satisfy the CRITERIA. You don't have to force the step to close if the customer isn't ready — stay conversational and the workflow will advance once the criteria are clearly met.
            </ConversationWorkflow>
            """;
    }
}
