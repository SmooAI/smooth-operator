---
'@smooai/smooth-operator-server': minor
'@smooai/smooth-operator': minor
---

TS server: resolve `send_message.skill` server-side (Rust PR #338 parity).

The TS server carried the `skill` field on the wire and ignored it — its own 1.39.0 changelog said so outright ("the TS / Python / Go / .NET servers ignore the field for now"). It now resolves the skill and composes it into the turn:

- **`skills.ts`** — `isValidSkillName` (ASCII alphanumerics + `-`/`_`, ≤128 chars, making `..`, `/`, `\` and NUL *unrepresentable* rather than filtered), `stripFrontmatter` (drops the discovery-metadata YAML so the model sees only instructions; unterminated frontmatter is returned untouched rather than swallowing the file), `skillSection`, and `resolveSection`.
- **`SkillResolver`** — the host seam, via `serve({ skillResolver })`.
- **`DirSkillResolver`** — `<root>/<name>/SKILL.md` over the `:`-separated roots in `SMOOTH_SKILLS_DIR`, first root wins. `serve()` prefers an explicit resolver, else `DirSkillResolver.fromEnv()`, mirroring Rust's `install_skill_resolver_from_env`. Neither ⇒ no resolver, so a multi-tenant deploy never serves host skills by accident.
- **Fail-CLOSED**, unlike `images`: an unresolvable skill returns `error { code: "SKILL_NOT_FOUND" }` and the turn does not run — resolved *before* the 202 ack, so a client never sees "accepted" for a turn that was never going to happen. A blank/whitespace `skill` is treated as absent, matching Rust's trim-then-filter.
- The body is appended to the **system prompt**, last, so it is the most salient instruction into the turn while the persisted user message stays exactly what the user typed — skill prose never accumulates in history to be replayed on every later turn.

Tests: all five Rust `skills.rs` unit tests ported under their Rust names, plus over-the-socket coverage for fail-closed (asserting the model is never called), system-prompt-not-user-message placement (including that frontmatter never reaches the model), and blank-as-absent with a resolver installed. 254 server tests green.

Backward compatible: an absent `skill` field is byte-for-byte the previous behavior.
