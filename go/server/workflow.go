package server

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// SMOODEV-590 — per-agent config: freeform instructions + a structured conversation
// workflow (plus optional greeting / personality / tool allow-list).
//
// This file ports the monorepo's general-agent workflow behavior
// (packages/backend/src/ai/graphs/general-agent/workflow.ts) and per-agent
// `instructions` handling, and mirrors the pushed TS / Python server siblings for this
// polyglot repo (agentConfig.ts + workflow.ts). It is deliberately free of LLM / I/O so
// the resolution + rendering logic unit-tests trivially; the one LLM-touching function,
// judgeWorkflowStep, lives in workflow_judge.go.
//
// The server resolves each conversation's agent id into an AgentConfig (via an
// AgentConfigResolver) and folds it into the system prompt for that agent's turns — so
// two agents in the same org behave differently instead of all using one generic org
// persona. An un-configured agent falls back to the server/org default prompt + full
// tool set, so behavior is unchanged.

// Workflow field bounds, mirrored from the authoritative Zod schema in
// packages/schemas/src/agents/agent.ts. A config that violates any bound is treated as
// ABSENT (degrades to the default freeform flow) rather than crashing a turn.
const (
	workflowGoalMax     = 1000
	workflowMaxSteps    = 20
	workflowStepIDMax   = 64
	workflowIntentMax   = 500
	workflowCriteriaMax = 1000
)

// ConversationWorkflowStep is one step in a structured conversation workflow. Mirrors
// the ConversationWorkflowStep Zod schema (id/intent/criteria/next).
type ConversationWorkflowStep struct {
	ID       string `json:"id"`
	Intent   string `json:"intent"`
	Criteria string `json:"criteria"`
	Next     string `json:"next,omitempty"`
}

// ConversationWorkflow is a goal plus an ordered list of steps the agent works through.
// When set, it turns the freeform prompt into a directed intent/criteria sequence a
// post-turn judge advances. Mirrors the ConversationWorkflow Zod schema.
type ConversationWorkflow struct {
	Goal  string                     `json:"goal"`
	Steps []ConversationWorkflowStep `json:"steps"`
}

// AgentConfig is the per-agent config that shapes an agent's conversations. Every field
// is optional — an agent may set only Instructions, only a Workflow, or nothing (the
// server then falls back to its base/org prompt + full tool set).
type AgentConfig struct {
	// Instructions is the agent's freeform system-prompt body (agents.instructions.prompt).
	// When set it becomes the primary persona, AUGMENTED by (not replacing) the base
	// prompt's grounding rules — see assembleSystemPrompt.
	Instructions string
	// Workflow, when non-nil, drives a stepped guided-agency flow: the current step's
	// intent + criteria are injected into the system prompt and a post-turn judge
	// advances the step.
	Workflow *ConversationWorkflow
	// Greeting is an optional first-reply greeting woven into the persona section.
	Greeting string
	// Personality is an optional short descriptor folded into the persona section.
	Personality string
	// AllowedTools, when non-empty, is an allow-list restricting this agent to those
	// tool names (by exact name); empty ⇒ all server tools available.
	AllowedTools []string
}

// AgentConfigResolver resolves a session's agent id into its AgentConfig. A nil return
// (agent unknown / no per-agent config) means the server uses its base/org default
// prompt + full tool set, so behavior is unchanged for un-configured agents. This is the
// config-DELIVERY seam, mirroring the server's other pluggable seams (AuthVerifier): the
// reference ships an in-memory resolver; a real deployment plugs in one backed by the
// monorepo `agents` table. The create_conversation_session payload carries only an agent
// id (per the spec), so config is resolved server-side by that id, never off the wire.
type AgentConfigResolver interface {
	// Resolve returns the config for agentID, or nil when the agent has none.
	Resolve(ctx context.Context, agentID string) (*AgentConfig, error)
}

// StaticAgentConfigResolver is an AgentConfigResolver backed by a fixed agentID→config
// map — the reference implementation (tests / local use). A real deployment reads the
// agents table instead. Safe for concurrent use (read-only after construction).
type StaticAgentConfigResolver struct {
	byID map[string]*AgentConfig
}

// NewStaticAgentConfigResolver builds a resolver over a fixed agentID→config map.
func NewStaticAgentConfigResolver(configs map[string]*AgentConfig) *StaticAgentConfigResolver {
	byID := make(map[string]*AgentConfig, len(configs))
	for id, cfg := range configs {
		byID[id] = cfg
	}
	return &StaticAgentConfigResolver{byID: byID}
}

// Resolve returns the agent's config, or nil when unset.
func (r *StaticAgentConfigResolver) Resolve(_ context.Context, agentID string) (*AgentConfig, error) {
	return r.byID[agentID], nil
}

// ParseAgentConfig tolerantly parses a raw agent record (the shape stored in the
// monorepo `agents` table: `instructions` jsonb `{prompt}` or a bare string,
// `conversation_workflow` jsonb, `greeting`, `personality`, `tool_config`) into an
// AgentConfig. Malformed sub-fields are dropped INDIVIDUALLY — a broken
// conversation_workflow doesn't discard a valid instructions.prompt — and it never
// errors. Returns nil only when nothing usable is present, so an un-configured or
// garbage record degrades to the default flow.
func ParseAgentConfig(raw json.RawMessage) *AgentConfig {
	if len(raw) == 0 {
		return nil
	}
	var obj map[string]json.RawMessage
	if err := json.Unmarshal(raw, &obj); err != nil {
		return nil
	}

	cfg := AgentConfig{}
	populated := false

	// instructions: either the jsonb {"prompt": string} or a bare string.
	if instr, ok := obj["instructions"]; ok {
		if prompt := parseInstructionsPrompt(instr); prompt != "" {
			cfg.Instructions = prompt
			populated = true
		}
	}

	// conversation_workflow (snake) / conversationWorkflow (camel) — tolerant parse.
	wfRaw := obj["conversation_workflow"]
	if len(wfRaw) == 0 {
		wfRaw = obj["conversationWorkflow"]
	}
	if wf := parseWorkflow(wfRaw); wf != nil {
		cfg.Workflow = wf
		populated = true
	}

	if s := parseJSONString(obj["greeting"]); s != "" {
		cfg.Greeting = s
		populated = true
	}
	if s := parseJSONString(obj["personality"]); s != "" {
		cfg.Personality = s
		populated = true
	}

	// tool_config (snake) / allowedTools (camel) — a string array allow-list.
	toolsRaw := obj["tool_config"]
	if len(toolsRaw) == 0 {
		toolsRaw = obj["allowedTools"]
	}
	if names := parseStringArray(toolsRaw); len(names) > 0 {
		cfg.AllowedTools = names
		populated = true
	}

	if !populated {
		return nil
	}
	return &cfg
}

// parseInstructionsPrompt extracts a prompt from the instructions jsonb: a bare non-empty
// string, or the `.prompt` of an object. Anything else yields "".
func parseInstructionsPrompt(raw json.RawMessage) string {
	if len(raw) == 0 {
		return ""
	}
	if s := parseJSONString(raw); s != "" {
		return s
	}
	var obj struct {
		Prompt string `json:"prompt"`
	}
	if err := json.Unmarshal(raw, &obj); err != nil {
		return ""
	}
	return strings.TrimSpace(obj.Prompt)
}

// parseJSONString decodes raw as a JSON string and trims it; "" for anything else.
func parseJSONString(raw json.RawMessage) string {
	if len(raw) == 0 {
		return ""
	}
	var s string
	if err := json.Unmarshal(raw, &s); err != nil {
		return ""
	}
	return strings.TrimSpace(s)
}

// parseStringArray decodes raw as a JSON array of non-empty strings; nil for anything
// else (a non-array, or a mix — non-string elements are dropped).
func parseStringArray(raw json.RawMessage) []string {
	if len(raw) == 0 {
		return nil
	}
	var arr []string
	if err := json.Unmarshal(raw, &arr); err != nil {
		return nil
	}
	out := make([]string, 0, len(arr))
	for _, s := range arr {
		if s != "" {
			out = append(out, s)
		}
	}
	if len(out) == 0 {
		return nil
	}
	return out
}

// parseWorkflow decodes + validates the conversation_workflow jsonb. Returns nil when the
// JSON is absent, malformed, or violates the schema bounds. A returned workflow is
// guaranteed well-formed, so the rest of the workflow code skips re-validation.
func parseWorkflow(raw json.RawMessage) *ConversationWorkflow {
	if len(raw) == 0 {
		return nil
	}
	var wf ConversationWorkflow
	if err := json.Unmarshal(raw, &wf); err != nil {
		return nil
	}
	if !validWorkflow(&wf) {
		return nil
	}
	return &wf
}

// validWorkflow reports whether wf satisfies the schema bounds. A workflow that fails any
// check is treated as absent (the turn degrades to the freeform default), never a crash.
func validWorkflow(wf *ConversationWorkflow) bool {
	if wf == nil {
		return false
	}
	if strings.TrimSpace(wf.Goal) == "" || len(wf.Goal) > workflowGoalMax {
		return false
	}
	if len(wf.Steps) == 0 || len(wf.Steps) > workflowMaxSteps {
		return false
	}
	ids := make(map[string]struct{}, len(wf.Steps))
	for i := range wf.Steps {
		s := &wf.Steps[i]
		if strings.TrimSpace(s.ID) == "" || len(s.ID) > workflowStepIDMax {
			return false
		}
		if _, dup := ids[s.ID]; dup {
			return false // step ids must be unique — next/pointer resolution keys on them.
		}
		ids[s.ID] = struct{}{}
		if strings.TrimSpace(s.Intent) == "" || len(s.Intent) > workflowIntentMax {
			return false
		}
		if strings.TrimSpace(s.Criteria) == "" || len(s.Criteria) > workflowCriteriaMax {
			return false
		}
		if len(s.Next) > workflowStepIDMax {
			return false
		}
	}
	return true
}

// resolveCurrentStep returns the current step for a workflow + pointer. Port of the TS
// resolveCurrentStep: a matching currentStepID wins; an empty/unknown pointer resolves to
// the first step (fresh start); no steps ⇒ nil.
func resolveCurrentStep(wf *ConversationWorkflow, currentStepID string) *ConversationWorkflowStep {
	if wf == nil || len(wf.Steps) == 0 {
		return nil
	}
	if currentStepID != "" {
		for i := range wf.Steps {
			if wf.Steps[i].ID == currentStepID {
				return &wf.Steps[i]
			}
		}
	}
	return &wf.Steps[0]
}

// nextStep computes the step to advance to once current is satisfied. Port of the TS
// nextStep: explicit current.Next (if it resolves) wins, else the following array
// element, else nil (terminal step).
func nextStep(wf *ConversationWorkflow, current *ConversationWorkflowStep) *ConversationWorkflowStep {
	if wf == nil || current == nil {
		return nil
	}
	if current.Next != "" {
		for i := range wf.Steps {
			if wf.Steps[i].ID == current.Next {
				return &wf.Steps[i]
			}
		}
	}
	idx := -1
	for i := range wf.Steps {
		if wf.Steps[i].ID == current.ID {
			idx = i
			break
		}
	}
	if idx == -1 || idx+1 >= len(wf.Steps) {
		return nil
	}
	return &wf.Steps[idx+1]
}

// renderWorkflowPromptSection renders the current step as a <ConversationWorkflow> block
// for the system prompt, or "" when no workflow is configured (so callers can
// interpolate unconditionally). Port of the TS renderWorkflowPromptSection.
func renderWorkflowPromptSection(wf *ConversationWorkflow, currentStepID string) string {
	step := resolveCurrentStep(wf, currentStepID)
	if wf == nil || step == nil {
		return ""
	}
	stepNumber := 1
	for i := range wf.Steps {
		if wf.Steps[i].ID == step.ID {
			stepNumber = i + 1
			break
		}
	}
	total := len(wf.Steps)
	return fmt.Sprintf(`<ConversationWorkflow>
GOAL: %s

CURRENT STEP (%d/%d): %s
INTENT: %s
CRITERIA: %s

Focus this turn on the CURRENT STEP. Pursue the INTENT and aim to satisfy the CRITERIA. You don't have to force the step to close if the user isn't ready — stay conversational and the workflow will advance once the criteria are clearly met.
</ConversationWorkflow>`, wf.Goal, stepNumber, total, step.ID, step.Intent, step.Criteria)
}

// assembleSystemPrompt builds the effective system prompt for a turn from the server's
// base prompt, the per-agent config, and the conversation's current workflow step. Port
// of the TS assembleSystemPrompt (augment, not replace):
//   - nil / empty config ⇒ base unchanged (behavior identical to before per-agent config);
//   - personality (when set) leads;
//   - the agent's instructions become the primary persona, FOLLOWED by the base prompt so
//     its grounding / behavior rules always apply (instructions augment, never discard,
//     the base);
//   - greeting and the rendered workflow step follow.
func assembleSystemPrompt(base string, cfg *AgentConfig, currentStepID string) string {
	if cfg == nil {
		return base
	}

	var sections []string
	if cfg.Personality != "" {
		sections = append(sections, "<Personality>\n"+cfg.Personality+"\n</Personality>")
	}
	if cfg.Instructions != "" {
		sections = append(sections, "<AgentInstructions>\n"+cfg.Instructions+"\n</AgentInstructions>")
	}
	sections = append(sections, base)
	if cfg.Greeting != "" {
		sections = append(sections, "<GreetingAwareness>\nIf this is your first reply in the conversation, open with a natural, brief variant of: \""+cfg.Greeting+"\" — then address the user's message. Do not repeat it verbatim on later turns.\n</GreetingAwareness>")
	}
	if section := renderWorkflowPromptSection(cfg.Workflow, currentStepID); section != "" {
		sections = append(sections, section)
	}
	return strings.Join(sections, "\n\n")
}

// filterTools returns the subset of tools whose names are in the config's AllowedTools
// allow-list. An empty allow-list (or nil config) returns tools unchanged, so an
// un-configured agent keeps the full server tool set.
func filterTools(tools []core.Tool, cfg *AgentConfig) []core.Tool {
	if cfg == nil || len(cfg.AllowedTools) == 0 {
		return tools
	}
	allowed := make(map[string]struct{}, len(cfg.AllowedTools))
	for _, name := range cfg.AllowedTools {
		allowed[name] = struct{}{}
	}
	out := make([]core.Tool, 0, len(tools))
	for _, t := range tools {
		if _, ok := allowed[t.Name()]; ok {
			out = append(out, t)
		}
	}
	return out
}

// advanceStep returns the currentStepID after a turn given the judge verdict. Port of the
// TS advanceStep: only a "yes" advances (via nextStep, or stays on a terminal step);
// every other verdict — including the failure-tolerant "skipped" — stays on the current
// step. A nil workflow / unresolved step returns "" (no tracking).
func advanceStep(wf *ConversationWorkflow, currentStepID string, verdict WorkflowVerdict) string {
	current := resolveCurrentStep(wf, currentStepID)
	if current == nil {
		return ""
	}
	if verdict == VerdictYes {
		if adv := nextStep(wf, current); adv != nil {
			return adv.ID
		}
	}
	return current.ID
}
