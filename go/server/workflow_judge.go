package server

import (
	"context"
	"encoding/json"
	"fmt"
	"regexp"
	"strings"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// SMOODEV-590 — the post-turn workflow judge.
//
// After each turn, a cheap LLM call decides whether the CURRENT STEP's criteria were
// satisfied this turn. "yes" advances the workflow; anything else stays put and the next
// turn renders the same step. Port of the monorepo's workflowJudgeNode
// (packages/backend/src/ai/graphs/general-agent/nodes/workflow-judge.ts) and the TS /
// Python server siblings' judgeStep.
//
// Failure-tolerant by design: any judge error (no client, model error, unparseable
// reply) resolves to VerdictSkipped, which advanceStep treats as "stay on the current
// step" — a judge failure never freezes or jumps the conversation.

// WorkflowVerdict is the judge's decision for a turn. "skipped" means no workflow, an
// empty reply, or a judge failure — the workflow stays on the current step.
type WorkflowVerdict string

const (
	// VerdictYes — the current step's criteria are clearly satisfied → advance.
	VerdictYes WorkflowVerdict = "yes"
	// VerdictNo — not satisfied → stay on the current step.
	VerdictNo WorkflowVerdict = "no"
	// VerdictMaybe — partial/ambiguous progress → stay and try again next turn.
	VerdictMaybe WorkflowVerdict = "maybe"
	// VerdictSkipped — nothing to judge or the judge failed → stay (fail-safe).
	VerdictSkipped WorkflowVerdict = "skipped"
)

// DefaultJudgeModel is the cheap fast-tier model the workflow judge uses when the server
// configures no explicit judge model. Matches this server's default main model
// (claude-haiku-4-5) — already a fast tier, so the extra per-turn latency/cost stays low.
const DefaultJudgeModel = "claude-haiku-4-5"

// workflowJudgeSystemPrompt instructs the judge model. Mirrors the sibling servers' judge
// prompt: yes only when criteria are objectively met, and reply as a JSON verdict object.
const workflowJudgeSystemPrompt = `You are a conversation-workflow judge. Given the CURRENT STEP's intent + criteria and the most recent agent reply, decide whether the step was satisfied this turn.

Rules:
- "yes" -> the criteria are clearly satisfied on the basis of this turn.
- "no" -> not satisfied, or the agent moved away from the step.
- "maybe" -> partial / ambiguous progress. The workflow stays on the current step and tries again next turn.
- Only answer "yes" when the criteria are objectively met. It is fine to stay on a step for multiple turns.

Respond with ONLY a JSON object: {"verdict":"yes"|"no"|"maybe"}.`

// verdictWordRe extracts the first standalone yes/no/maybe token from a non-JSON reply.
var verdictWordRe = regexp.MustCompile(`\b(yes|no|maybe)\b`)

// judgeWorkflowStep runs the post-turn judge for one turn and returns the verdict. It is
// a no-op fast path (VerdictSkipped) when there is no client, no workflow, no resolvable
// current step, or no agent reply to judge — matching the sibling servers' early-outs. On
// any judge error it returns VerdictSkipped so the workflow stays on the current step.
//
// model is the cheap judge model; "" falls back to DefaultJudgeModel.
func judgeWorkflowStep(ctx context.Context, client core.ChatClient, model string, wf *ConversationWorkflow, currentStepID, userMessage, reply string) WorkflowVerdict {
	if client == nil || wf == nil {
		return VerdictSkipped
	}
	current := resolveCurrentStep(wf, currentStepID)
	if current == nil {
		return VerdictSkipped
	}
	if strings.TrimSpace(reply) == "" {
		// The agent produced no customer-facing reply — nothing to judge.
		return VerdictSkipped
	}
	if model == "" {
		model = DefaultJudgeModel
	}

	human := fmt.Sprintf(`GOAL: %s

CURRENT STEP (%s):
  intent: %s
  criteria: %s

LAST USER MESSAGE:
%s

AGENT REPLY:
%s`, wf.Goal, current.ID, current.Intent, current.Criteria, orNone(userMessage), reply)

	resp, err := client.Chat(ctx, core.ChatRequest{
		Model: model,
		Messages: []core.ChatMessage{
			{Role: "system", Content: workflowJudgeSystemPrompt},
			{Role: "user", Content: human},
		},
		Temperature: 0,
		MaxTokens:   200,
	})
	if err != nil {
		// Never freeze the conversation on a judge failure — stay on the current step.
		return VerdictSkipped
	}
	return parseVerdict(resp.Content)
}

// parseVerdict maps a judge reply to a verdict. It prefers a JSON {"verdict":"..."}
// object (what the judge is asked for), falling back to a standalone yes/no/maybe word
// scan so a model that ignores the JSON instruction still advances the workflow. An
// unrecognized reply is VerdictSkipped (stay), never a spurious advance.
func parseVerdict(content string) WorkflowVerdict {
	trimmed := strings.TrimSpace(content)
	if trimmed == "" {
		return VerdictSkipped
	}
	var parsed struct {
		Verdict string `json:"verdict"`
	}
	if err := json.Unmarshal([]byte(trimmed), &parsed); err == nil {
		switch WorkflowVerdict(strings.ToLower(strings.TrimSpace(parsed.Verdict))) {
		case VerdictYes:
			return VerdictYes
		case VerdictNo:
			return VerdictNo
		case VerdictMaybe:
			return VerdictMaybe
		}
	}
	if m := verdictWordRe.FindStringSubmatch(strings.ToLower(trimmed)); m != nil {
		return WorkflowVerdict(m[1])
	}
	return VerdictSkipped
}

// orNone returns "(none)" for an empty user message, matching the sibling judges' fallback.
func orNone(s string) string {
	if strings.TrimSpace(s) == "" {
		return "(none)"
	}
	return s
}
