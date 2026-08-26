---
"@smooai/smooth-operator": patch
---

fix(server): a pending write-confirmation survives the pod that parked it (th-db0816)

The write-confirmation park was a channel into a turn running on ONE pod. A
visitor whose refresh reconnected them to a different replica — or whose pod
rolled mid-park — got `NO_PENDING_CONFIRMATION` for an approval the agent had
just asked them to give, and the approved write was silently lost. With 2-6
replicas and no session affinity, that is the expected outcome for any
reconnect, not a rare race.

The confirmation bridge now mirrors every park into the session's durable
`metadata.pendingConfirmation` (tool name + arguments + prompt + requestedAt)
through the same `SessionUpdate.metadata` write-through the session registry
already uses: storage is the truth, the in-process channel map is the same-pod
fast path. `confirm_tool_action` that finds no live sender reads the record
FRESH from storage and resolves it there:

- **Approved** → the record is retired first, fail-closed (a record that cannot
  be cleared could execute a write twice, so a failed clear surfaces as a
  retryable storage error instead of proceeding), then a continuation turn runs
  through the normal `send_message` path. A one-shot, server-side pre-approval
  for exactly the recorded tool lets the re-issued call execute instead of
  parking a second time — it is granted only by the resolving handler and never
  readable from the wire, so a client cannot smuggle a confirmation bypass into
  a frame.
- **Denied** → the record is retired and the confirm is acked; the parked tool
  never runs anywhere (a dead park cannot execute, and a still-parked twin on
  another pod resolves to its timeout rejection).
- Records expire on the same 300s clock as the in-process park, so a
  pod-death orphan cannot authorize a stale write later.

The same-pod path is unchanged (sender fed, turn resumes in place) except that
it now also retires the durable record immediately on resolution.

`rust/smooth-operator-server/tests/durable_confirmation.rs` drives the real
`handle_frame` on TWO `AppState`s sharing one storage adapter — the same
two-instance shape as the session-registry fix. The approval test asserts its
premise (pod B holds no live park), that the continuation actually executes the
gated tool WITHOUT a second `write_confirmation_required`, and that the record
is retired; the decline test asserts retire-without-a-turn; the negative
control proves `NO_PENDING_CONFIRMATION` is still reachable with no record, so
the positive tests ride on the record path and nothing else. Positive control:
with the durable fallback disabled, both cross-instance tests fail reproducing
the exact production error, while the negative control still passes.

No wire-protocol change.
