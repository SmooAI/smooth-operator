"""SMOODEV-590 — structured conversation-workflow helpers + post-turn judge.

A ``ConversationWorkflow`` turns a freeform system prompt into a directed
sequence of intent/criteria steps. The runner renders the *current* step's
intent + criteria into the system prompt, then a cheap post-turn judge call
decides whether the step's criteria were met this turn → advance to ``next``
(or the sequential next element).

The Python analog of the monorepo's ``general-agent/workflow.ts`` (pure step
helpers) and ``nodes/workflow-judge.ts`` (the post-turn judge). The step
helpers are pure + I/O-free so they are trivial to unit-test; the judge is the
only async piece and is failure-tolerant (any error → stay on the current step,
never freeze the conversation).

Opt-in: an agent with no ``conversation_workflow`` behaves exactly as before
(freeform prompt from ``instructions``).
"""

from __future__ import annotations

import json
from typing import Any, Literal

from .agent_config import ConversationWorkflow, ConversationWorkflowStep

#: The judge verdict. ``skipped`` means "no workflow / nothing to evaluate".
WorkflowJudgeVerdict = Literal["yes", "no", "maybe", "skipped"]

#: Cheap fast-tier model for the yes/no/maybe judge decision. Matches the engine's
#: default (``claude-haiku-4-5``) so the extra per-turn latency + cost stays minimal
#: (the analog of the TS ``getFastModel`` fast slot).
WORKFLOW_JUDGE_MODEL = "claude-haiku-4-5"

_JUDGE_SYSTEM_PROMPT = (
    "You are a conversation-workflow judge. Given the CURRENT STEP's intent + criteria "
    "and the most recent agent reply, decide whether the step was satisfied this turn.\n\n"
    "Rules:\n"
    '- "yes" -> the criteria are clearly satisfied on the basis of this turn.\n'
    '- "no" -> not satisfied, or the agent moved away from the step.\n'
    '- "maybe" -> partial/ambiguous progress. The workflow stays on the current step and tries again next turn.\n'
    '- A brief, informal, or terse user answer that addresses the step\'s question satisfies it '
    '(e.g. "a four", "sure", "not really") — mark "yes"; do not hold out for elaboration or exact wording.\n'
    '- It is OK to stay on a step for multiple turns, but never require the user to re-confirm something they already said.\n\n'
    'Reply with ONLY a JSON object: {"verdict": "yes"|"no"|"maybe", "reason": "<one sentence>"}.'
)


def resolve_current_step(
    workflow: ConversationWorkflow | None,
    current_step_id: str | None,
) -> ConversationWorkflowStep | None:
    """Resolve the current step for a workflow + pointer.

    - ``current_step_id`` matches a step by id → that step.
    - Pointer empty or unknown → the first step (fresh start).
    - Workflow has no steps → ``None``.
    """
    if workflow is None or not workflow.steps:
        return None
    if current_step_id:
        for step in workflow.steps:
            if step.id == current_step_id:
                return step
    return workflow.steps[0]


def next_step(workflow: ConversationWorkflow, current: ConversationWorkflowStep) -> ConversationWorkflowStep | None:
    """The step to advance to once ``current`` is satisfied.

    Preference order (mirrors the TS ``nextStep``):
      1. Explicit ``current.next`` if it resolves to a known step id.
      2. The element immediately following ``current`` in ``steps``.
      3. ``None`` — workflow complete (terminal step).
    """
    if current.next:
        for step in workflow.steps:
            if step.id == current.next:
                return step
    for idx, step in enumerate(workflow.steps):
        if step.id == current.id:
            nxt = idx + 1
            return workflow.steps[nxt] if nxt < len(workflow.steps) else None
    return None


def render_workflow_prompt_section(
    workflow: ConversationWorkflow | None,
    current_step_id: str | None,
) -> str:
    """Render the current step as a ``<ConversationWorkflow>`` block for the system
    prompt. Empty string when no workflow is configured, so the caller can
    interpolate unconditionally (mirrors ``renderWorkflowPromptSection``)."""
    step = resolve_current_step(workflow, current_step_id)
    if workflow is None or step is None:
        return ""
    idx = next((i for i, s in enumerate(workflow.steps) if s.id == step.id), -1)
    step_number = idx + 1 if idx >= 0 else 1
    total = len(workflow.steps)
    return (
        "<ConversationWorkflow>\n"
        f"GOAL: {workflow.goal}\n\n"
        f"CURRENT STEP ({step_number}/{total}): {step.id}\n"
        f"INTENT: {step.intent}\n"
        f"CRITERIA: {step.criteria}\n\n"
        "Focus this turn on the CURRENT STEP: pursue the INTENT directly in this reply — ask the "
        "step's question now. The user has already agreed to be here; never re-ask for permission, "
        "re-confirm readiness, or repeat a question they have already answered — acknowledge briefly "
        "and move forward. Stay conversational; the workflow advances once the CRITERIA are met.\n"
        "</ConversationWorkflow>"
    )


def _parse_verdict(content: str) -> WorkflowJudgeVerdict:
    """Extract a ``yes``/``no``/``maybe`` verdict from the judge's reply. Tolerant of
    prose or code fences around the JSON; unrecognized → ``no`` (stay put, don't
    advance on an ambiguous reply)."""
    text = content.strip()
    start, end = text.find("{"), text.rfind("}")
    if start != -1 and end > start:
        try:
            verdict = json.loads(text[start : end + 1]).get("verdict")
            if verdict in ("yes", "no", "maybe"):
                return verdict
        except (json.JSONDecodeError, TypeError, AttributeError):
            pass
    lowered = text.lower()
    if "yes" in lowered:
        return "yes"
    if "maybe" in lowered:
        return "maybe"
    return "no"


async def judge_workflow_step(
    chat_client: Any,
    workflow: ConversationWorkflow | None,
    current: ConversationWorkflowStep | None,
    user_message: str,
    reply: str,
    *,
    model: str = WORKFLOW_JUDGE_MODEL,
) -> WorkflowJudgeVerdict:
    """Post-turn judge: did the agent satisfy ``current``'s criteria this turn?

    Returns ``skipped`` when there is nothing to judge (no workflow / no step / no
    reply). Otherwise a cheap fast-model call returns ``yes``/``no``/``maybe``.
    Failure-tolerant: any judge error → ``no`` (stay on the current step) — the
    conversation never freezes on a judge outage (mirrors ``workflow-judge.ts``)."""
    if workflow is None or current is None or not reply.strip():
        return "skipped"
    if chat_client is None:
        return "no"

    human_prompt = (
        f"GOAL: {workflow.goal}\n\n"
        f"CURRENT STEP ({current.id}):\n"
        f"  intent: {current.intent}\n"
        f"  criteria: {current.criteria}\n\n"
        f"LAST USER MESSAGE:\n{user_message or '(none)'}\n\n"
        f"AGENT REPLY:\n{reply}\n\n"
        "Return {verdict, reason}."
    )
    try:
        response = await chat_client.chat.completions.create(
            model=model,
            temperature=0,
            max_tokens=200,
            messages=[
                {"role": "system", "content": _JUDGE_SYSTEM_PROMPT},
                {"role": "user", "content": human_prompt},
            ],
        )
        content = response.choices[0].message.content or ""
    except Exception:
        # Never freeze the conversation on a judge failure — stay on the current step.
        return "no"
    return _parse_verdict(content)
