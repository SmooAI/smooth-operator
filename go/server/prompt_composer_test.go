package server

import (
	"fmt"
	"strings"
	"testing"
)

// PromptComposer seam (SMOODEV-3798) — parity with the Rust reference's
// prompt_composer tests.

func testSections() PromptSections {
	return PromptSections{
		Base:             "BASE",
		BaseSource:       BaseSourceAgent,
		Greeting:         "GREETING",
		Workflow:         "WORKFLOW",
		Skill:            "SKILL",
		SuggestedReplies: "TRAILER",
		ConversationID:   "c1",
	}
}

func TestPromptComposerDefaultKeepsHistoricalOrder(t *testing.T) {
	if got := RenderPrompt(nil, testSections()); got != "BASE\n\nGREETING\n\nWORKFLOW\n\nSKILL\n\nTRAILER" {
		t.Fatalf("default order = %q", got)
	}
}

func TestPromptComposerAbsentSectionsSkipped(t *testing.T) {
	s := testSections()
	s.Greeting, s.Workflow, s.Skill = "", "", ""
	if got := RenderPrompt(nil, s); got != "BASE\n\nTRAILER" {
		t.Fatalf("got %q", got)
	}
}

func TestPromptComposerJoinDropsBlank(t *testing.T) {
	if got := JoinSections([]string{"a", "", "  ", "b"}); got != "a\n\nb" {
		t.Fatalf("got %q", got)
	}
}

type safetyComposer struct{}

func (safetyComposer) Compose(s PromptSections) []string {
	return append(DefaultSections(s), fmt.Sprintf("SAFETY(source=%s, authed=%t, conv=%s)", s.BaseSource, s.SessionAuthenticated, s.ConversationID))
}

func TestPromptComposerHostAppendsLastAndSeesContext(t *testing.T) {
	got := RenderPrompt(safetyComposer{}, testSections())
	if !strings.HasPrefix(got, "BASE\n\nGREETING") || !strings.HasSuffix(got, "TRAILER\n\nSAFETY(source=agent, authed=false, conv=c1)") {
		t.Fatalf("got %q", got)
	}
}

type reorderComposer struct{}

func (reorderComposer) Compose(s PromptSections) []string {
	return []string{s.Base, s.Workflow, s.SuggestedReplies}
}

func TestPromptComposerHostMayReorder(t *testing.T) {
	if got := RenderPrompt(reorderComposer{}, testSections()); got != "BASE\n\nWORKFLOW\n\nTRAILER" {
		t.Fatalf("got %q", got)
	}
}

// The default composition of the agent parts equals assembleSystemPrompt.
func TestPromptComposerAgentPartsMatchAssembleSystemPrompt(t *testing.T) {
	cfg := &AgentConfig{Instructions: "Be Ada.", Personality: "Warm.", Greeting: "Hi!"}
	for _, first := range []bool{true, false} {
		p := agentPromptParts("SERVER", cfg, "", first)
		got := RenderPrompt(nil, PromptSections{Base: p.Base, Greeting: p.Greeting, Workflow: p.Workflow})
		if want := assembleSystemPrompt("SERVER", cfg, "", first); got != want {
			t.Fatalf("first=%t: composed %q, want %q", first, got, want)
		}
	}
}
