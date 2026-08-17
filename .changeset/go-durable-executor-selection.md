---
"@smooai/smooth-operator": patch
---

feat(go): env-gated durable-executor selection seam on the Go server turn path (th-137b91, Q parity)

The Go server drove every turn by calling `SmoothAgent.RunStream` directly, so there was no single
place a durable backend (ADR-030) could plug in — parity gap with the Rust server's `turn_executor`.
`TurnRunner.Run` now drives the turn through the engine's `AgentExecutor` seam:
`turnExecutor(r.executor, os.Getenv("SMOOTH_AGENT_DURABLE_EXECUTOR")).ExecuteStreaming(...)`.

The durable backend is DEPENDENCY-INJECTED via the new `TurnRunner.executor` field (nil by default),
so the server binary keeps no compile-time dependency on any Temporal package. Selection mirrors the
Rust `durable_requested` opt-in exactly (`1`/`true`/`on`/`yes`, case- and whitespace-insensitive):
the injected executor is used only when the env opts in AND one was supplied; otherwise the
zero-infra `InProcessExecutor`, a verbatim delegation to `RunStream`, so a default deployment behaves
exactly as before. Requesting durable mode with nothing injected warns and falls back rather than
silently pretending the turn is durable. Unit tests cover the full selection matrix with a fake
injected executor.
