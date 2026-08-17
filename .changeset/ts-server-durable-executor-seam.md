---
"@smooai/smooth-operator-server": minor
---

feat(server): env-gated durable-executor selection seam on the turn path (th-137b91, Q parity)

The TS server ran every turn by calling `SmoothAgent.runStream` directly, with no
place for a durable backend to plug in — unlike the Rust server, whose
`turn_executor` selects the executor in one spot. This adds the sibling seam:
`turnExecutor(injected?)` returns an injected `AgentExecutor` verbatim, else the
engine's zero-infra `InProcessExecutor` (a verbatim delegation to `runStream`, so
behavior is unchanged when nothing opts in). Setting `SMOOTH_AGENT_DURABLE_EXECUTOR`
without supplying an executor warns and falls back rather than silently pretending
the turn is durable.

`TurnRunner` now takes an optional `executor` and runs the turn through
`executor.executeStreaming(agent, …)` instead of `agent.runStream(…)` — the one
place ADR-030's durable backend (e.g. `@smooai/smooth-operator-temporal`'s
`TemporalAgentExecutor`) plugs in. The backend is injected as a **parameter**, so
this server keeps **no dependency** on the Temporal package.
