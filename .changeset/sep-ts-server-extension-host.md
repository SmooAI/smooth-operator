---
'@smooai/smooth-operator': minor
---

SEP — the TypeScript operator server now hosts extensions (`ui/*` producer),
mirroring the Rust reference (`rust/smooth-operator-server/src/extensions.rs`).

`typescript/server` wires the engine `ExtensionHost`
(`@smooai/smooth-operator-core/extension`) into each turn: with
`SMOOTH_EXTENSIONS_ALLOW` set (a default-deny, comma-separated trust allow-list)
it discovers `extension.toml` extensions, spawns them as JSON-RPC/ndjson
subprocesses, and registers their `<ext>.<tool>` tools into the turn's tool set
BEFORE the per-agent `enabled_tools` filter — so an allow-list drops them exactly
like a built-in (SMOODEV-590 parity). A `ConfirmUiProvider` bridges an
extension's `ui/confirm` onto the existing `write_confirmation_required` /
`confirm_tool_action` frames via the session-keyed `ConfirmationRegistry`; every
other `ui/*` degrades headless (render-only → `{}`, select/input → `{cancelled}`).
The host and its subprocesses are torn down at turn end. Unset
`SMOOTH_EXTENSIONS_ALLOW` (the default) builds no host — behavior is unchanged.
