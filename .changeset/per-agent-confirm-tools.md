---
'@smooai/smooth-operator': minor
---

Per-agent write-confirmation (HITL) patterns. `AgentConfig` gains a
`ConfirmToolPatterns` field so a multi-agent host can gate tools behind a
`confirm_tool_action` round-trip per agent instead of sharing the single global
`ConfirmTools` DI singleton. The dispatcher uses the per-agent patterns when the
agent specifies them (an explicit empty list disables gating for that agent) and
falls back to the global `ConfirmTools` when it doesn't — fully backward
compatible.
