package server

import "strings"

// Prompt composition seam (SMOODEV-3798) — parity with the Rust reference's
// smooth_operator_server::prompt_composer.
//
// A PromptComposer assembles the turn's system prompt from its ordered sections plus
// who is asking and which agent answers. The dispatcher drops blank sections and joins
// the rest with a blank line (JoinSections). With no composer installed it uses
// DefaultPromptComposer, which keeps this server's historical order: base (personality,
// agent instructions, server prompt), first-turn greeting, workflow step, then the
// invoked skill.

// BaseSource records where PromptSections.Base came from.
type BaseSource string

const (
	// BaseSourceAgent is the answering agent's own instructions.
	BaseSourceAgent BaseSource = "agent"
	// BaseSourceOrgPersona is the org's saved persona override.
	BaseSourceOrgPersona BaseSource = "orgPersona"
	// BaseSourceDefaultPersona is the host's server-wide system prompt.
	BaseSourceDefaultPersona BaseSource = "defaultPersona"
	// BaseSourceBuiltIn is the server's built-in prompt.
	BaseSourceBuiltIn BaseSource = "builtIn"
)

// PromptSections is every input to one turn's system prompt. An empty string means
// the section does not apply to this turn.
type PromptSections struct {
	Base                 string
	BaseSource           BaseSource
	Greeting             string
	Workflow             string
	Skill                string
	SuggestedReplies     string
	Agent                *AgentConfig
	Access               AccessContext
	SessionAuthenticated bool
	ConversationID       string
}

// PromptComposer assembles the turn's system prompt. Return the sections in order;
// blank sections are dropped and the rest joined with "\n\n".
type PromptComposer interface {
	Compose(sections PromptSections) []string
}

// DefaultPromptComposer keeps the historical order: base, greeting, workflow, skill,
// suggested replies.
type DefaultPromptComposer struct{}

// DefaultSections returns the default ordered sections, for hosts that extend rather
// than reorder.
func DefaultSections(s PromptSections) []string {
	return []string{s.Base, s.Greeting, s.Workflow, s.Skill, s.SuggestedReplies}
}

// Compose implements PromptComposer.
func (DefaultPromptComposer) Compose(s PromptSections) []string { return DefaultSections(s) }

// JoinSections drops blank sections and joins the rest with a blank line.
func JoinSections(sections []string) string {
	kept := make([]string, 0, len(sections))
	for _, s := range sections {
		if strings.TrimSpace(s) != "" {
			kept = append(kept, s)
		}
	}
	return strings.Join(kept, "\n\n")
}

// RenderPrompt composes sections with composer (nil → the default) and joins them.
func RenderPrompt(composer PromptComposer, sections PromptSections) string {
	if composer == nil {
		composer = DefaultPromptComposer{}
	}
	return JoinSections(composer.Compose(sections))
}
