---
'@smooai/smooth-operator': patch
---

SMOODEV-2259 — per-agent SEP extension enablement: `AgentBehaviorConfig` now carries
`enabled_extensions` (parsed from the `agents.extension_config` jsonb, camelCase
`enabledExtensions[{extensionId, enabled, config}]`), and the operator server's extension
host intersects the server allowlist (`SMOOTH_EXTENSIONS_ALLOW`) with the per-agent enabled
extension ids.

Fail-closed for resolved agents: any agent that resolves to a config (exists in the agents
DB) but enables no extensions loads ZERO extensions, even when the server allowlist is
non-empty — extensions can intercept & mutate tool calls, so a public agent must never
silently inherit one. Backward-compatible when no per-agent config resolves at all
(bare/standalone operator): the server allowlist alone decides, unchanged. The Postgres
resolver now keys "no per-agent config" off row existence (not `is_empty()`), so a
found-but-blank agent is distinguishable from an unknown one; the `extension_config` column
read degrades to `None` on a standalone deploy whose table predates the column (no migration
ordering dependency).
