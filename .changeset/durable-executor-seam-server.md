---
"@smooai/smooth-operator": patch
---

feat(server): run turns through the engine's `AgentExecutor` seam (ADR-030)

`runner::run_streaming_turn` no longer calls `Agent::run_with_channel` directly. It
resolves an executor via the new `turn_executor()` and drives the turn through the
engine's `AgentExecutor` seam instead. With the default `InProcessExecutor` that IS
the same call — a verbatim delegation — so every deployed turn is byte-for-byte what
it was, and the full server test suite passes unchanged.

What the indirection buys is a single place to select a durable backend. The runner
wraps the turn in roughly two hundred lines of event translation, confirmation- and
interaction-bridge teardown, and OTel span emission; without the seam, a durable
backend would have to be threaded through all of it. Now it plugs in at one function.

Durable mode is opt-in via `SMOOTH_AGENT_DURABLE_EXECUTOR` and off by default. Today
it still resolves to in-process and logs a warning, because the durable backend lives
in `smooai-smooth-operator-temporal`, which is `publish = false` in the engine repo:
this crate consumes the engine from crates.io and is itself published, so it can take
neither a git nor a path dependency on it. Warning and falling back is deliberate — a
turn the client believes will survive a disconnect, but won't, is worse than no
durable mode at all.

To be clear about what this does and does not fix: it does **not** yet stop a parked
write-approval from dying on a browser refresh. That park is in-process and bounded at
about five minutes, and it stays that way until the Temporal executor plugs in here.
When it does, the engine's `AgentTurnWorkflow` gates approval-required tools on
durable `approve_tool` / `deny_tool` signals, so the pending decision lives in workflow
history rather than in the process serving the socket. This change is the precondition
for that, not the fix itself.

Also raises the `smooai-smooth-operator-core` floor to 1.7.10, the first published core
carrying the `executor` module.
