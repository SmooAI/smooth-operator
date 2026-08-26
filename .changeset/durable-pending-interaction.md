---
"@smooai/smooth-operator": patch
---

fix(server): a pending Rich Interaction survives the pod that parked it (th-db0816)

The interaction sibling of the durable-confirmation fix. A raise's park is a
channel into a turn on ONE pod, so a visitor whose refresh reconnected them to
another replica — or whose pod rolled — got `NO_PENDING_INTERACTION` for the
card they were just shown, and the identity they typed evaporated.

The interaction bridge now persists every raise into the session's durable
`metadata.pendingInteraction` (interaction id + kind + spec — the full
validation contract), retired when the turn ends. `submit_interaction` with no
live park validates against that record exactly as it would against the park
(mismatched `interactionId` still rejected, per-field validation still routed
to the kind's server-side validator, invalid submits leave the record for a
retry) and resolves it there: submitted values retire the record fail-closed
and then run the kind's host effect; declined retires it with an ack. No
continuation turn — the host effect is the durable outcome, and a dead raise's
model acknowledgment is forgone rather than fabricated.

`attach_session_identity` now also writes through to storage: a captured
contact (name/email/phone) used to live only in one pod's map, so any pod roll
forgot a visitor who had just introduced themselves — even on the same pod.

Two-instance tests (`tests/durable_interactions.rs`) drive the real
`handle_frame` on two `AppState`s over one storage adapter: submit-on-the-other-
pod attaches the identity durably and retires the record, decline retires it,
mismatched ids are still rejected, and the negative control proves
`NO_PENDING_INTERACTION` is still reachable with no record. Positive control:
with the durable read disabled, both cross-instance tests fail reproducing the
production error while the negative control still passes.

No wire-protocol change.
