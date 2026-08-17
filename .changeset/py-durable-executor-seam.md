---
"@smooai/smooth-operator": patch
---

feat(python-server): env-gated durable-executor selection seam (th-137b91, Q parity)

The Python server ran every turn by calling `SmoothAgent.run_stream` directly, so there was no single
place a durable backend (ADR-030) could be selected — unlike the Rust server's `turn_executor`
(`runner.rs`). `TurnRunner` now routes each turn through the engine's `AgentExecutor` seam, chosen
once in `select_turn_executor`: a durable backend is dependency-injected as an opaque `AgentExecutor`
(so the server keeps no hard dependency on the Temporal package), and it is used only when
`SMOOTH_AGENT_DURABLE_EXECUTOR` opts in (`1/true/on/yes`). With nothing injected — the default — the
turn runs on `InProcessExecutor`, a verbatim delegation to `run_stream`, so behavior is unchanged.
Asking for durable mode with nothing injected warns and falls back rather than silently pretending a
turn is durable. `durable_requested` is split out for a testable parse. Tests cover the parse table,
the selection logic, and two real turns driven through a fake injected executor.
