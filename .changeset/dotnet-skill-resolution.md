---
'@smooai/smooth-operator-server': minor
---

.NET server: resolve `send_message.skill` server-side (Rust PR #338 parity).

The C# server had the generated `SendMessageRequest.Skill` field but ignored it — the same staging `images` went through. It now resolves the skill and composes it into the turn, closing the last text-path gap with the Rust reference:

- **`Skills`** — `IsValidSkillName` (ASCII alphanumerics + `-`/`_`, ≤128 chars, so `..`, `/`, `\` and NUL are *unrepresentable* rather than filtered), `StripFrontmatter` (drops the discovery-metadata YAML block, leaving only the instructions the model should see; unterminated frontmatter is returned untouched rather than swallowing the file), `SkillSection` (the `## Skill: <name>` framing), and `ResolveSectionAsync`.
- **`ISkillResolver`** — the host seam, injected via the `FrameDispatcher` constructor (the C# analog of Rust's `AppState::with_skill_resolver`).
- **`DirSkillResolver`** — the working default: `<root>/<name>/SKILL.md` over the `:`-separated roots in `SMOOTH_SKILLS_DIR`, first root wins. The ASP.NET host prefers a DI-registered `ISkillResolver` and otherwise falls back to `DirSkillResolver.FromEnv()`, mirroring Rust's `install_skill_resolver_from_env`. Unset ⇒ no resolver installed, so a multi-tenant deploy never serves host skills by accident.
- **Fail-CLOSED**, unlike `images`: an unresolvable skill returns `error { code: "SKILL_NOT_FOUND" }` and the turn does not run. A caller that asked for a code-review recipe and silently got a freeform answer has no way to tell. A blank/whitespace `skill` is treated as absent, matching Rust's trim-then-filter.
- The resolved body goes to the **system prompt**, appended last so it is the most salient instruction into the turn — the persisted user message stays exactly what the user typed, so skill prose never accumulates in conversation history and gets replayed every later turn.

Tests: `SkillTests` ports all five Rust `skills.rs` unit tests under their Rust names, plus dispatcher-level coverage for fail-closed `SKILL_NOT_FOUND`, system-prompt-not-user-message placement, blank-skill-as-absent, and unchanged behavior when the field is absent. `RecordingChatClient` was promoted out of `FileTransferTests` into a shared `TestChatClients.cs` so both suites use one double.

Backward compatible: an absent `skill` field is byte-for-byte the previous behavior. Source-only — the engine stays the published NuGet.
