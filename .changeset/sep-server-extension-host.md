---
'@smooai/smooth-operator': minor
---

SEP — the Rust operator server now hosts extensions (`ui/*` producer).

The reference operator server (`smooth-operator-server`) wires the engine
`ExtensionHost` into each turn: with `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny
allowlist — the server has no interactive trust prompt), it discovers
`extension.toml` extensions, spawns them as JSON-RPC/ndjson subprocesses, and
attaches the host to the agent. An extension's tools land in the turn's
`ToolRegistry` and flow through the same per-agent `enabled_tools` filtering +
authLevel gating as built-ins (SMOODEV-590), and its hooks/events run in the
agent loop.

`ui/confirm` is projected onto the existing `write_confirmation_required` /
`confirm_tool_action` HITL frames — the same out-of-band bridge the native
write-tool `ConfirmationHook` uses, so a hosted extension's confirm prompt pauses
and resumes the turn end-to-end. Every other `ui/*` degrades headless (only the
`confirm` capability is advertised at handshake). Unconfigured (empty allowlist),
no host is built and behavior is byte-for-byte unchanged.

This is the first operator server to host extensions. The other four polyglot
servers (TypeScript, Python, Go, .NET) have the agent-loop + HITL landing pad
wired but their engine cores have no SEP `ExtensionHost` yet — porting it to each
engine is tracked as follow-up work.
