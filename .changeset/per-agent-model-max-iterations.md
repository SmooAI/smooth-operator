---
'@smooai/smooth-operator': minor
---

SMOODEV-2172 — per-agent `model` and `max_iterations` overrides. `AgentBehaviorConfig`
now carries `model: Option<String>` (per-agent gateway model id) and
`max_iterations: Option<u32>` (per-agent agent-loop cap), parsed from optional
`agents.model` (text) and `agents.max_iterations` (integer) row values. Blank models
are ignored; `max_iterations` is clamped to `1..=64` with a `warn` on clamp.

At turn time the operator server threads both through: the model resolves highest-wins
as per-turn `send_message.model` (Smooth Modes) → per-agent `agents.model` →
`SMOOTH_AGENT_MODEL`; the loop cap resolves per-agent `agents.max_iterations` →
`SMOOTH_AGENT_MAX_ITERATIONS`. `None` at every layer falls back to the global env
default exactly as before, so a standalone deploy is byte-for-byte unchanged. The
reference Postgres adapter reads both columns tolerantly — a DB predating them degrades
to the global default (no migration-ordering dependency).
