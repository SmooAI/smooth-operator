---
"@smooai/smooth-extension-sdk": minor
"@smooai/smooth-operator": patch
---

SEP Phase 4 (spec + SDK) — commands, flags, shortcuts, and session actions.

**Spec.** New `command-complete.schema.json` (argument autocomplete). `session.schema.json` now carries the dispatch `context` on every params object (the wire form of the command-tier + epoch guard the host enforces) and adds `send_user_message` (`deliver_as` steer/follow_up/next_turn). `initialize.schema.json` gains a `flags` delivery map on the params and a `shortcuts` list (+ `ShortcutRegistration`) on the registrations. New conformance fixtures for command/complete, session send_user_message/append_entry, shortcuts, and flag delivery; new `$invalid` cases proving `context` is required on a session action and `value` on a completion. The reference `echo.mjs` registers a command + shortcut and answers command/execute + command/complete.

**SDK.** `smooth.registerCommand` (with an optional `complete` completer), `registerFlag` (+ `smooth.getFlag`), and `registerShortcut`. Command handlers receive a `CommandContext` bound to their command-tier context, exposing `session.sendMessage` / `sendUserMessage` / `appendEntry`, `ui`, `hasUI`, and `args`. `createTestHost` gains `runCommand`, `completeCommand`, and a `session/*` service that enforces the same command-tier guard the engine does (event-tier → -32003), recording every session call for assertions. `runConformance` now replays command/execute + command/complete.

**Demo.** `plan-mode` — the flagship extension that exercises phases 2–4 together: a `--plan` flag and a `/plan` command toggle plan mode; a `tool_call` intercept blocks write/edit/apply_patch/bash while it is on; each toggle pushes a `set_widget` render block and persists an LLM-invisible `appendEntry`, so the state survives a hot reload (the flag re-seeds it, the transcript keeps the history).
