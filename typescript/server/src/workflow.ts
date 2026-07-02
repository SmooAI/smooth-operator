/**
 * SMOODEV-590 — Structured conversation workflow (TypeScript server port).
 *
 * A `ConversationWorkflow` turns a freeform system prompt into a directed sequence
 * of intent/criteria steps. The server renders the current step's intent + criteria
 * into the system prompt, then a cheap post-turn judge call decides whether the
 * step criteria were met this turn → advance to the next step.
 *
 * The TS parity of the monorepo LangGraph general-agent (`workflow.ts` /
 * `nodes/workflow-judge.ts`) and the Rust server's workflow module. The pure
 * helpers here are free of LLM / I/O so they unit-test trivially; {@link judgeStep}
 * is the one LLM-touching function and is failure-tolerant by design.
 *
 * Opt-in: an agent with no `conversationWorkflow` behaves exactly as before
 * (freeform prompt from its instructions / the org default).
 */
import type { ChatClientLike } from '@smooai/smooth-operator-core';

/**
 * A single step in a structured conversation workflow. Mirrors the authoritative
 * `ConversationWorkflowStep` schema (`@smooai/schemas/agents/agent`): id 1..64,
 * intent 1..500, criteria 1..1000, optional `next` step id.
 */
export interface ConversationWorkflowStep {
    id: string;
    intent: string;
    criteria: string;
    next?: string;
}

/** goal + ordered steps (1..20) the agent works through. */
export interface ConversationWorkflow {
    goal: string;
    steps: ConversationWorkflowStep[];
}

/** The judge verdict. `skipped` means "no workflow / nothing to evaluate / judge failed". */
export type WorkflowJudgeVerdict = 'yes' | 'no' | 'maybe' | 'skipped';

const MAX_STEPS = 20;
const MAX_ID = 64;
const MAX_INTENT = 500;
const MAX_CRITERIA = 1000;

function isNonEmptyString(value: unknown, max: number): value is string {
    return typeof value === 'string' && value.trim().length > 0 && value.length <= max;
}

/**
 * Tolerantly parse a raw (DB jsonb / wire) value into a {@link ConversationWorkflow}.
 *
 * Returns `undefined` for anything malformed — a missing goal, no steps, too many
 * steps, or a step missing id/intent/criteria — so a bad config degrades to the
 * org-default freeform flow rather than crashing a session. Never throws.
 */
export function parseWorkflow(raw: unknown): ConversationWorkflow | undefined {
    if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return undefined;
    const obj = raw as Record<string, unknown>;
    if (!isNonEmptyString(obj.goal, 1000)) return undefined;
    if (!Array.isArray(obj.steps) || obj.steps.length < 1 || obj.steps.length > MAX_STEPS) return undefined;

    const steps: ConversationWorkflowStep[] = [];
    for (const rawStep of obj.steps) {
        if (typeof rawStep !== 'object' || rawStep === null) return undefined;
        const s = rawStep as Record<string, unknown>;
        if (!isNonEmptyString(s.id, MAX_ID) || !isNonEmptyString(s.intent, MAX_INTENT) || !isNonEmptyString(s.criteria, MAX_CRITERIA)) {
            return undefined;
        }
        const step: ConversationWorkflowStep = { id: s.id, intent: s.intent, criteria: s.criteria };
        if (typeof s.next === 'string' && s.next.trim().length > 0 && s.next.length <= MAX_ID) step.next = s.next;
        steps.push(step);
    }
    // Duplicate step ids make `next`/pointer resolution ambiguous → treat as malformed.
    if (new Set(steps.map((s) => s.id)).size !== steps.length) return undefined;

    return { goal: obj.goal, steps };
}

/**
 * Resolve the current step for a workflow + pointer.
 *
 * - `currentStepId` matching a step by id → that step.
 * - empty / unknown pointer → the first step (fresh start).
 * - no steps → null.
 */
export function resolveCurrentStep(
    workflow: ConversationWorkflow | null | undefined,
    currentStepId: string | null | undefined,
): ConversationWorkflowStep | null {
    if (!workflow || workflow.steps.length === 0) return null;
    if (currentStepId) {
        const found = workflow.steps.find((s) => s.id === currentStepId);
        if (found) return found;
    }
    return workflow.steps[0] ?? null;
}

/**
 * The step to advance to once `current` is satisfied. Preference order:
 *   1. explicit `current.next` if it resolves to a known step id;
 *   2. the element immediately after `current` in the array;
 *   3. `null` — workflow complete (terminal step).
 */
export function nextStep(workflow: ConversationWorkflow, current: ConversationWorkflowStep): ConversationWorkflowStep | null {
    if (current.next) {
        const explicit = workflow.steps.find((s) => s.id === current.next);
        if (explicit) return explicit;
    }
    const idx = workflow.steps.findIndex((s) => s.id === current.id);
    if (idx === -1) return null;
    return workflow.steps[idx + 1] ?? null;
}

/**
 * Render the current step as a `<ConversationWorkflow>` block for the system prompt.
 * Returns an empty string when no workflow is configured so callers can interpolate
 * unconditionally.
 */
export function renderWorkflowPromptSection(workflow: ConversationWorkflow | null | undefined, currentStepId: string | null | undefined): string {
    const step = resolveCurrentStep(workflow, currentStepId);
    if (!workflow || !step) return '';
    const idx = workflow.steps.findIndex((s) => s.id === step.id);
    const stepNumber = idx >= 0 ? idx + 1 : 1;
    const total = workflow.steps.length;
    return `<ConversationWorkflow>
GOAL: ${workflow.goal}

CURRENT STEP (${stepNumber}/${total}): ${step.id}
INTENT: ${step.intent}
CRITERIA: ${step.criteria}

Focus this turn on the CURRENT STEP. Pursue the INTENT and aim to satisfy the CRITERIA. You don't have to force the step to close if the user isn't ready — stay conversational and the workflow will advance once the criteria are clearly met.
</ConversationWorkflow>`;
}

/** Default cheap model slot for the judge (matches the server's default main model). */
export const DEFAULT_JUDGE_MODEL = 'gpt-4o-mini';

/** Extract the first `yes` / `no` / `maybe` token from a judge reply. */
function parseVerdict(content: string | null | undefined): 'yes' | 'no' | 'maybe' | undefined {
    if (!content) return undefined;
    // Prefer a JSON `{ "verdict": "..." }` shape, but fall back to a bare-word scan
    // so a model that ignores the JSON instruction still advances the workflow.
    try {
        const parsed = JSON.parse(content) as { verdict?: unknown };
        if (parsed.verdict === 'yes' || parsed.verdict === 'no' || parsed.verdict === 'maybe') return parsed.verdict;
    } catch {
        // not JSON — fall through to the word scan
    }
    const match = content.toLowerCase().match(/\b(yes|no|maybe)\b/);
    return match ? (match[1] as 'yes' | 'no' | 'maybe') : undefined;
}

export interface JudgeStepInput {
    workflow: ConversationWorkflow;
    current: ConversationWorkflowStep;
    userMessage: string;
    reply: string;
    /** The cheap model id to judge with (defaults to {@link DEFAULT_JUDGE_MODEL}). */
    model?: string;
}

/**
 * Post-turn judge: decide whether the current step's criteria were satisfied this
 * turn, using a cheap fast-tier model call on the shared chat client.
 *
 * Failure-tolerant: any judge error or unparseable verdict returns `skipped`, so the
 * caller stays on the current step and the conversation never freezes or skips ahead.
 */
export async function judgeStep(chatClient: ChatClientLike, input: JudgeStepInput): Promise<WorkflowJudgeVerdict> {
    const { workflow, current, userMessage, reply, model } = input;
    // Nothing the agent said → nothing to judge; stay put.
    if (reply.trim().length === 0) return 'skipped';

    const systemPrompt = `You are a conversation-workflow judge. Given the CURRENT STEP's intent + criteria and the most recent agent reply, decide whether the step was satisfied this turn.

Rules:
- "yes" → the criteria are clearly satisfied on the basis of this turn.
- "no" → not satisfied, or the agent moved away from the step.
- "maybe" → partial / ambiguous progress. The workflow stays on the current step and tries again next turn.
- Only answer "yes" when the criteria are objectively met. It is fine to stay on a step for multiple turns.

Respond with ONLY a JSON object: {"verdict":"yes"|"no"|"maybe"}.`;

    const humanPrompt = `GOAL: ${workflow.goal}

CURRENT STEP (${current.id}):
  intent: ${current.intent}
  criteria: ${current.criteria}

LAST USER MESSAGE:
${userMessage || '(none)'}

AGENT REPLY:
${reply}`;

    try {
        const response = await chatClient.chat.completions.create({
            model: model ?? DEFAULT_JUDGE_MODEL,
            temperature: 0,
            max_tokens: 200,
            messages: [
                { role: 'system', content: systemPrompt },
                { role: 'user', content: humanPrompt },
            ],
        });
        const verdict = parseVerdict(response.choices[0]?.message.content);
        return verdict ?? 'skipped';
    } catch {
        // Never freeze the conversation on a judge failure — stay on the current step.
        return 'skipped';
    }
}

/**
 * Advance a workflow pointer given a fresh judge verdict.
 *
 * `yes` → the id of {@link nextStep} (or stays on the terminal step when there is no
 * next). Anything else → stays on `current`. Returns the step id to persist as the
 * conversation's `currentStepId`.
 */
export function advanceStep(workflow: ConversationWorkflow, current: ConversationWorkflowStep, verdict: WorkflowJudgeVerdict): string {
    if (verdict === 'yes') {
        const advance = nextStep(workflow, current);
        return advance ? advance.id : current.id;
    }
    return current.id;
}
