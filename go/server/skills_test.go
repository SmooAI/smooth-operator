package server

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// Skill resolution parity (th-ebe27d / Rust PR #338): the wire carries INTENT
// (send_message.skill), the server resolves it and composes the body into the turn's
// system prompt, and an unknown skill fails CLOSED with SKILL_NOT_FOUND rather than
// silently degrading into an unskilled answer.
//
// The first group mirrors rust/smooth-operator-server/src/skills.rs case for case; the
// rest cover the dispatcher wiring.

// writeSkill lays down <root>/<name>/SKILL.md, with frontmatter unless body is bare.
func writeSkill(t *testing.T, root, name, body string, frontmatter bool) {
	t.Helper()
	dir := filepath.Join(root, name)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	text := body
	if frontmatter {
		text = "---\nname: " + name + "\n---\n" + body + "\n"
	}
	if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte(text), 0o644); err != nil {
		t.Fatalf("write: %v", err)
	}
}

func TestIsValidSkillNameRejectsTraversalAndSeparators(t *testing.T) {
	valid := []string{"code-review", "add_show", strings.Repeat("a", 128)}
	for _, n := range valid {
		if !IsValidSkillName(n) {
			t.Errorf("IsValidSkillName(%q) = false, want true", n)
		}
	}
	invalid := []string{"", "..", "../../etc/passwd", "a/b", `a\b`, "a b", strings.Repeat("a", 129), "ok\x00.md"}
	for _, n := range invalid {
		if IsValidSkillName(n) {
			t.Errorf("IsValidSkillName(%q) = true, want false", n)
		}
	}
}

func TestStripFrontmatterOnlyWhenWellFormed(t *testing.T) {
	cases := []struct{ in, want string }{
		{"---\nname: x\ndescription: y\n---\nBody here\n", "Body here\n"},
		// No frontmatter → untouched.
		{"Body here\n", "Body here\n"},
		// Unterminated → untouched (don't swallow the file).
		{"---\nname: x\n", "---\nname: x\n"},
		// A `---` mid-body (a markdown rule) after real frontmatter still closes at the
		// FIRST fence, which is the frontmatter's.
		{"---\nname: x\n---\nintro\n\n---\n\nmore\n", "intro\n\n---\n\nmore\n"},
	}
	for _, c := range cases {
		if got := stripFrontmatter(c.in); got != c.want {
			t.Errorf("stripFrontmatter(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestDirResolverReadsFirstMatchingRoot(t *testing.T) {
	tmp := t.TempDir()
	high, low := filepath.Join(tmp, "high"), filepath.Join(tmp, "low")
	writeSkill(t, high, "greet", "HIGH BODY", true)
	writeSkill(t, low, "greet", "LOW BODY", true)

	r := NewDirSkillResolver([]string{high, low})
	if body, ok := r.ResolveSkill(context.Background(), "greet"); !ok || body != "HIGH BODY" {
		t.Fatalf("resolve(greet) = %q, %v; want HIGH BODY, true", body, ok)
	}
	if _, ok := r.ResolveSkill(context.Background(), "nope"); ok {
		t.Error("resolve(nope) succeeded; want unknown")
	}
	// Traversal can't escape the root even though the file exists one level up.
	if _, ok := r.ResolveSkill(context.Background(), "../low/greet"); ok {
		t.Error("traversal resolved; the name must make it unrepresentable")
	}
	// A missing first root falls through to the next one's copy.
	r = NewDirSkillResolver([]string{filepath.Join(tmp, "missing"), low})
	if body, ok := r.ResolveSkill(context.Background(), "greet"); !ok || body != "LOW BODY" {
		t.Fatalf("fallthrough resolve = %q, %v; want LOW BODY, true", body, ok)
	}
}

func TestPathListParsingIsOffWhenEmpty(t *testing.T) {
	if DirSkillResolverFromPathList("") != nil {
		t.Error("empty list produced a resolver")
	}
	if DirSkillResolverFromPathList("  : ") != nil {
		t.Error("whitespace-only list produced a resolver")
	}
	r := DirSkillResolverFromPathList("/a: /b :")
	if r == nil || len(r.roots) != 2 || r.roots[0] != "/a" || r.roots[1] != "/b" {
		t.Fatalf("roots = %+v, want [/a /b]", r)
	}
}

func TestResolveSkillSectionComposesAndReportsUnknown(t *testing.T) {
	tmp := t.TempDir()
	writeSkill(t, tmp, "review", "Check the diff.", false)
	r := NewDirSkillResolver([]string{tmp})

	section, ok := ResolveSkillSection(context.Background(), r, "review")
	if !ok {
		t.Fatal("known skill did not resolve")
	}
	if !strings.HasPrefix(section, "## Skill: review\n") || !strings.HasSuffix(section, "Check the diff.") {
		t.Fatalf("section = %q", section)
	}
	if _, ok := ResolveSkillSection(context.Background(), r, "unknown"); ok {
		t.Error("unknown skill resolved")
	}
	// No resolver installed ⇒ every skill is unknown.
	if _, ok := ResolveSkillSection(context.Background(), nil, "review"); ok {
		t.Error("nil resolver resolved a skill")
	}
}

func TestDirSkillResolverFromEnv(t *testing.T) {
	tmp := t.TempDir()
	writeSkill(t, tmp, "greet", "ENV BODY", true)
	t.Setenv(SkillsDirEnv, tmp)
	r := DirSkillResolverFromEnv()
	if r == nil {
		t.Fatal("env resolver not built")
	}
	if body, ok := r.ResolveSkill(context.Background(), "greet"); !ok || body != "ENV BODY" {
		t.Fatalf("env resolve = %q, %v", body, ok)
	}
	t.Setenv(SkillsDirEnv, "")
	if DirSkillResolverFromEnv() != nil {
		t.Error("empty env var produced a resolver")
	}
}

// A SKILL.md that is nothing but frontmatter has no instructions to follow, so it is
// unknown rather than an empty section appended to the prompt.
func TestEmptyBodyIsNotASkill(t *testing.T) {
	tmp := t.TempDir()
	writeSkill(t, tmp, "hollow", "", true)
	if _, ok := NewDirSkillResolver([]string{tmp}).ResolveSkill(context.Background(), "hollow"); ok {
		t.Error("frontmatter-only SKILL.md resolved as a skill")
	}
}

// ── dispatcher wiring ────────────────────────────────────────────────────────

// newSkillDispatcher builds a plain text-reply dispatcher with the given resolver
// installed the way the server installs it (post-construction, alongside hooks).
func newSkillDispatcher(resolver SkillResolver) (*FrameDispatcher, *core.MockLlmProvider) {
	mock := core.NewMockLlmProvider().PushText("done")
	d := NewFrameDispatcher(NewInMemorySessionStore(), mock, AccessContext{}, "BASE PROMPT", nil, nil, nil, nil, nil, "", nil, nil, nil, nil)
	d.skills = resolver
	return d, mock
}

// TestUnknownSkillFailsClosedBeforeTheAck: the turn either runs WITH the skill or does not
// run at all. The error must land INSTEAD of the 202, not after it, or the client holds an
// accepted turn that then errors.
func TestUnknownSkillFailsClosedBeforeTheAck(t *testing.T) {
	d, mock := newSkillDispatcher(NewDirSkillResolver([]string{t.TempDir()}))
	sid := createSessionForTest(t, d)

	sink := sendAndWait(d, `{"action":"send_message","requestId":"r-1","sessionId":"`+sid+`","message":"review this","skill":"nope"}`)

	ev := sink.find("error")
	if ev == nil {
		t.Fatal("no error event for an unknown skill")
	}
	errObj, _ := ev["error"].(map[string]any)
	if errObj["code"] != "SKILL_NOT_FOUND" {
		t.Fatalf("error code = %v, want SKILL_NOT_FOUND", errObj["code"])
	}
	if sink.find("immediate_response") != nil {
		t.Error("a rejected skill still produced a 202 ack")
	}
	if len(mock.Calls()) != 0 {
		t.Error("the turn ran despite an unknown skill")
	}
}

// Default deployment: no resolver ⇒ a skill field is a clean SKILL_NOT_FOUND, so a
// multi-tenant server never serves host skills by accident.
func TestSkillWithNoResolverInstalledIsNotFound(t *testing.T) {
	d, _ := newSkillDispatcher(nil)
	sid := createSessionForTest(t, d)

	sink := sendAndWait(d, `{"action":"send_message","requestId":"r-1","sessionId":"`+sid+`","message":"review this","skill":"review"}`)

	ev := sink.find("error")
	if ev == nil {
		t.Fatal("no error event with no resolver installed")
	}
	if errObj, _ := ev["error"].(map[string]any); errObj["code"] != "SKILL_NOT_FOUND" {
		t.Fatalf("error code = %v, want SKILL_NOT_FOUND", errObj["code"])
	}
}

// The whole point of the seam: the body lands in the SYSTEM PROMPT and the persisted/sent
// user message stays exactly what the user typed, so it is not replayed as context on
// every later turn.
func TestKnownSkillReachesTheSystemPromptNotTheUserMessage(t *testing.T) {
	tmp := t.TempDir()
	writeSkill(t, tmp, "review", "Be adversarial about the diff.", true)
	d, mock := newSkillDispatcher(NewDirSkillResolver([]string{tmp}))
	sid := createSessionForTest(t, d)

	sink := sendAndWait(d, `{"action":"send_message","requestId":"r-1","sessionId":"`+sid+`","message":"look at PR 12","skill":"review"}`)
	if sink.find("eventual_response") == nil {
		t.Fatal("turn did not complete")
	}

	system := systemPromptOf(t, mock)
	if !strings.Contains(system, "## Skill: review") || !strings.Contains(system, "Be adversarial about the diff.") {
		t.Fatalf("system prompt missing the skill section: %q", system)
	}
	// The base prompt survives — the skill is appended, not a replacement.
	if !strings.Contains(system, "BASE PROMPT") {
		t.Errorf("skill replaced the base prompt: %q", system)
	}
	for _, m := range mock.Calls()[0].Messages {
		if m.Role == "user" {
			if m.Content != "look at PR 12" {
				t.Fatalf("user message = %q, want the untouched text", m.Content)
			}
			if strings.Contains(m.Content, "adversarial") {
				t.Error("skill body leaked into the user message")
			}
		}
	}
}

// Back-compat: a turn without `skill` behaves byte-identically to before.
func TestNoSkillFieldLeavesThePromptUnchanged(t *testing.T) {
	tmp := t.TempDir()
	writeSkill(t, tmp, "review", "Be adversarial.", true)
	d, mock := newSkillDispatcher(NewDirSkillResolver([]string{tmp}))
	sid := createSessionForTest(t, d)

	sendAndWait(d, `{"action":"send_message","requestId":"r-1","sessionId":"`+sid+`","message":"hello"}`)

	if system := systemPromptOf(t, mock); strings.Contains(system, "## Skill:") {
		t.Fatalf("a skill-less turn got a skill section: %q", system)
	}
}

// An empty/whitespace `skill` is "no skill", not an unknown one — a client that always
// sends the field must not be unable to run an ordinary turn.
func TestBlankSkillIsIgnoredNotRejected(t *testing.T) {
	d, _ := newSkillDispatcher(NewDirSkillResolver([]string{t.TempDir()}))
	sid := createSessionForTest(t, d)

	sink := sendAndWait(d, `{"action":"send_message","requestId":"r-1","sessionId":"`+sid+`","message":"hello","skill":"   "}`)

	if sink.find("error") != nil {
		t.Fatal("a blank skill was rejected")
	}
	if sink.find("eventual_response") == nil {
		t.Fatal("turn did not complete")
	}
}

// The framing line is wire-visible behavior — it is what tells the model the skill applies
// to this turn — so it is pinned across every language, not incidental.
func TestSkillSectionShapeIsIdenticalAcrossLanguages(t *testing.T) {
	want := "## Skill: code-review\n\nThe user invoked this skill for this turn. Follow it.\n\nBODY"
	if got := SkillSection("code-review", "BODY"); got != want {
		t.Fatalf("SkillSection = %q, want %q", got, want)
	}
}

// A nil *DirSkillResolver assigned straight into the SkillResolver interface would be a
// non-nil interface holding a nil pointer, and ResolveSkillSection's `resolver == nil`
// guard would miss it — panicking instead of reporting SKILL_NOT_FOUND. The server's
// install path checks the concrete pointer for exactly this reason; pin it.
func TestUnsetEnvDoesNotProduceATypedNilResolver(t *testing.T) {
	t.Setenv(SkillsDirEnv, "")
	var resolver SkillResolver
	if env := DirSkillResolverFromEnv(); env != nil {
		resolver = env
	}
	if resolver != nil {
		t.Fatal("unset env produced a non-nil SkillResolver interface")
	}
	if _, ok := ResolveSkillSection(context.Background(), resolver, "review"); ok {
		t.Error("resolved through a nil resolver")
	}
}
