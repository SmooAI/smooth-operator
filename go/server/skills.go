package server

import (
	"context"
	"os"
	"path/filepath"
	"strings"
)

// Skill resolution — the engine side of `send_message.skill` (Rust PR #338).
//
// A *skill* is a named, reusable recipe (a markdown body). Before this seam every client
// resolved the skill itself and prepended the body to the message text, so the wire carried
// prose — and the body persisted into conversation history, where it was replayed as context
// on every later turn. Now the wire carries INTENT (`skill: "code-review"`) and the server
// composes it into the turn's system prompt, leaving the persisted user message exactly what
// the user typed.
//
// Two pieces, mirroring rust/smooth-operator-server/src/skills.rs:
//   - SkillResolver — the host seam, installed via WithSkillResolver.
//   - DirSkillResolver — the working default: <root>/<name>/SKILL.md over the roots in
//     SMOOTH_SKILLS_DIR (a ':'-separated list, first match wins). Unset ⇒ no resolver is
//     installed and any `skill` field is a clean SKILL_NOT_FOUND, so a multi-tenant deploy
//     never reads host skills by accident.

// SkillsDirEnv names the skill roots for DirSkillResolver: ':'-separated, searched in order.
const SkillsDirEnv = "SMOOTH_SKILLS_DIR"

// maxSkillNameLen bounds a skill name — see IsValidSkillName.
const maxSkillNameLen = 128

// SkillResolver resolves a skill name to its markdown body. ok=false means "unknown skill" —
// the dispatcher turns that into a SKILL_NOT_FOUND error and does NOT run the turn, so a
// typo'd skill never silently degrades into an unskilled answer.
type SkillResolver interface {
	// ResolveSkill returns the skill's markdown body, or ok=false when no such skill exists.
	ResolveSkill(ctx context.Context, name string) (body string, ok bool)
}

// SkillSection renders a resolved skill as a system-prompt section. The skill moved from the
// USER MESSAGE (where clients used to prepend it) to the SYSTEM PROMPT, so this framing line
// is what tells the model the skill applies to this turn.
func SkillSection(name, body string) string {
	return "## Skill: " + name + "\n\nThe user invoked this skill for this turn. Follow it.\n\n" + body
}

// IsValidSkillName reports whether name is a legal skill name. Deliberately strict: ASCII
// alphanumerics, '-' and '_' only. That is the kebab-case convention skills already use, and
// it makes path traversal ("..", "/", "\\", NUL) UNREPRESENTABLE rather than filtered — the
// name is joined onto a filesystem root by DirSkillResolver.
func IsValidSkillName(name string) bool {
	if name == "" || len(name) > maxSkillNameLen {
		return false
	}
	for _, c := range name {
		switch {
		case c >= 'a' && c <= 'z', c >= 'A' && c <= 'Z', c >= '0' && c <= '9', c == '-', c == '_':
		default:
			return false
		}
	}
	return true
}

// stripFrontmatter strips a leading YAML frontmatter block ("---" … "---") and returns the
// body. SKILL.md files carry frontmatter (description, triggers, allowed tools) that is
// discovery metadata, not instructions — the model should see only the body. Unterminated
// frontmatter is returned untouched rather than swallowing the file.
func stripFrontmatter(text string) string {
	if !strings.HasPrefix(text, "---\n") {
		return text
	}
	rest := text[4:]
	// The closing fence is a line that is exactly "---".
	for idx := strings.Index(rest, "---"); idx != -1; {
		atLineStart := idx == 0 || rest[idx-1] == '\n'
		restOfLine := rest[idx+3:]
		if atLineStart && strings.HasPrefix(restOfLine, "\n") {
			return strings.TrimLeft(restOfLine, "\n")
		}
		next := strings.Index(rest[idx+1:], "---")
		if next == -1 {
			break
		}
		idx += 1 + next
	}
	return text
}

// DirSkillResolver is the default resolver: reads <root>/<name>/SKILL.md, first root wins.
type DirSkillResolver struct {
	roots []string
}

// NewDirSkillResolver builds a resolver over an explicit list of roots, searched in order.
func NewDirSkillResolver(roots []string) *DirSkillResolver { return &DirSkillResolver{roots: roots} }

// DirSkillResolverFromEnv builds from SkillsDirEnv. nil when the var is unset or names no
// non-empty root, so the caller installs nothing and the feature stays off by default.
func DirSkillResolverFromEnv() *DirSkillResolver {
	raw, ok := os.LookupEnv(SkillsDirEnv)
	if !ok {
		return nil
	}
	return DirSkillResolverFromPathList(raw)
}

// DirSkillResolverFromPathList builds from a ':'-separated path list — the parsed half of
// DirSkillResolverFromEnv, so it is testable without touching the process environment.
//
// ponytail: ':' is hardcoded to match the Rust reference rather than os.PathListSeparator. On
// Windows that makes a drive-qualified root ("C:\\skills") unrepresentable; change it in every
// lane at once or they diverge.
func DirSkillResolverFromPathList(list string) *DirSkillResolver {
	var roots []string
	for _, part := range strings.Split(list, ":") {
		if p := strings.TrimSpace(part); p != "" {
			roots = append(roots, p)
		}
	}
	if len(roots) == 0 {
		return nil
	}
	return NewDirSkillResolver(roots)
}

// ResolveSkill implements SkillResolver.
func (r *DirSkillResolver) ResolveSkill(_ context.Context, name string) (string, bool) {
	if !IsValidSkillName(name) {
		return "", false
	}
	for _, root := range r.roots {
		// ponytail: blocking read. A SKILL.md is a few KB off local disk; revisit if a
		// resolver ever fronts network storage.
		raw, err := os.ReadFile(filepath.Join(root, name, "SKILL.md"))
		if err != nil {
			continue
		}
		if body := strings.TrimSpace(stripFrontmatter(string(raw))); body != "" {
			return body, true
		}
	}
	return "", false
}

// ResolveSkillSection resolves name through resolver and renders it as a system-prompt
// section. ok=false when there is no resolver installed or the skill is unknown — both are
// SKILL_NOT_FOUND to the client (the distinction is a deployment detail the caller should not
// have to guess at).
func ResolveSkillSection(ctx context.Context, resolver SkillResolver, name string) (string, bool) {
	if resolver == nil {
		return "", false
	}
	body, ok := resolver.ResolveSkill(ctx, name)
	if !ok {
		return "", false
	}
	return SkillSection(name, body), true
}
