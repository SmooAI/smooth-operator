---
'@smooai/smooth-operator': minor
---

`send_message` grows an optional `skill` field — the engine resolves and composes the skill, not the client.

Until now every client resolved a skill itself and prepended its markdown body to the message text. That put prose on the wire, and it persisted the skill body into conversation history, where it was replayed as context on every subsequent turn. The wire now carries the intent (`skill: "code-review"`) and the server does the work.

- **`skills::SkillResolver`** — the host seam, installed via `AppState::with_skill_resolver` or `LocalServerBuilder::skill_resolver`.
- **`skills::DirSkillResolver`** — the working default: `<root>/<name>/SKILL.md` over the `:`-separated roots in `SMOOTH_SKILLS_DIR`, first match wins, YAML frontmatter stripped. Unset ⇒ no resolver, so a multi-tenant deploy never serves host skills by accident.
- The resolved body becomes a **system-prompt section** for that turn only, so the persisted user message stays exactly what the user typed.
- **Fail-closed**, unlike `images`: an unresolvable skill returns `error { code: "SKILL_NOT_FOUND" }` and the turn does not run. Names are restricted to `[A-Za-z0-9_-]{1,128}`, making path traversal unrepresentable.

Backward compatible: an absent `skill` field is byte-for-byte the previous behavior. Rust is the reference implementation; the TS / Python / Go / .NET servers ignore the field for now (the same staging `images` is in).
