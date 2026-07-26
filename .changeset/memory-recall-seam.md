---
"@smooai/smooth-operator": patch
---

feat(engine): `StorageAdapter::memory_for_access` seam wires durable auto-recall into every turn

The engine core already supported memory auto-recall (`AgentConfig::with_memory` →
`build_context_messages` → `memory.recall(msg, 5)`), but the server never wired it:
`StorageAdapter` exposed `checkpoints()`/`knowledge()` but no memory accessor, and
the runner built each turn's `AgentConfig` without `.with_memory(...)`. This adds
`StorageAdapter::memory_for_access(&access) -> Option<Arc<dyn Memory>>` (defaulting to
`None`, so every existing backend is byte-for-byte unchanged — hosted auto-recall stays
a deliberate opt-in, not a side effect), and the runner now calls `.with_memory(...)`
whenever the adapter returns `Some`. The in-memory conformance adapter gains a
`with_memory(...)` builder + override so the seam is exercised end-to-end. This lights
up Big Smooth's durable auto-recall: its single-tenant SQLite adapter overrides
`memory_for_access` to return its store, so remembered preferences are injected into
every turn without the agent calling `recall` (th-374b27).
