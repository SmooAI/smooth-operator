---
"@smooai/smooth-operator": patch
---

feat(server): make the turn executor injectable per turn (ADR-030)

`TurnRequest` gains an optional `executor: Option<Arc<dyn AgentExecutor>>`. `None` —
every existing caller — runs the turn in-process exactly as before, so this changes
no behavior.

Two reasons it is a per-turn field rather than something the runner constructs or
holds process-globally:

- **It keeps Temporal out of this crate.** This crate publishes to crates.io, and
  cargo refuses to publish a crate declaring a git or path dependency even behind an
  off-by-default feature. With the executor injected, an unpublished deployment crate
  can build the durable executor and pass it in, and nothing Temporal-shaped ever
  appears in this manifest.
- **Durable mode is meant to be opted into per conversation**, which a process-global
  handle could not express. A process-global would also repeat the exact mismatch
  that makes the durable backend hard to adopt today — its activity worker holds one
  global registry while this server builds a per-turn, per-org, ACL-scoped one.

`turn_executor` now takes the injected value: supplied ⇒ used verbatim, and
`SMOOTH_AGENT_DURABLE_EXECUTOR` is not consulted. Nothing supplied ⇒ the in-process
executor, with the env var still warning rather than silently pretending a turn is
durable.

This is foundation only. It does **not** make a parked write-approval survive a
browser refresh — there is still no client-side `AgentExecutor` in
`smooai-smooth-operator-temporal` to inject, and building one is blocked on two open
design questions: a workflow-backed turn has no token-delta path (so it cannot feed
the runner's event translator), and `AgentTurnInput` carries neither prior messages
nor a per-turn tool registry.
