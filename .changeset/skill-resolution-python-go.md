---
"@smooai/smooth-operator": patch
---

feat(python,go): resolve `send_message.skill` server-side — the last two languages (th-ebe27d)

The skill seam (Rust #338) let the wire carry **intent** — `skill: "code-review"` —
instead of prose: the server resolves the name to its markdown body and composes it
into the turn's system prompt, so the persisted user message stays exactly what the
user typed and the body is not replayed as context on every later turn. Rust, C#
and TypeScript had it; the Python and Go servers ignored the field entirely, which
is worse than not supporting it — a client that asked for a skill got a confident
**unskilled** answer with no signal that anything was dropped.

Both now carry the full seam, mirroring `rust/smooth-operator-server/src/skills.rs`:
a `SkillResolver` host seam (`ServerState.skill_resolver` / `WithSkillResolver`) and
a `DirSkillResolver` default reading `<root>/<name>/SKILL.md` over the `:`-separated
roots in `SMOOTH_SKILLS_DIR`, first root wins. Unset ⇒ no resolver is installed and
any `skill` field is a clean `SKILL_NOT_FOUND`, so a multi-tenant deploy never
serves host skills by accident.

Two properties are load-bearing and tested as such:

- **Fail closed, before the ack.** An unresolvable skill emits `SKILL_NOT_FOUND`
  *instead of* the 202 and never starts a turn. Resolving after the ack would leave
  the client holding an accepted turn that then errors, and running the turn anyway
  is the silent-degradation bug above. A blank/whitespace `skill` is "no skill",
  not an unknown one, so a client that always sends the field still works.
- **Traversal is unrepresentable, not filtered.** The name is `[A-Za-z0-9_-]{1,128}`
  — matching the pattern `spec/actions/send-message.schema.json` already declared —
  before it is ever joined onto a filesystem root, so `../../etc/passwd`, `a/b`,
  `a\b` and an embedded NUL cannot round-trip into a path.

New conformance scenario `skill-unknown-error` pins the fail-closed contract across
**all five** servers: it needs no filesystem setup (the default server installs no
resolver), so it is the corpus's oracle for this seam rather than five per-language
opinions about it.

No wire-protocol change — the schema already carried the field.
