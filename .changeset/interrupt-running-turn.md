---
"@smooai/smooth-operator": patch
---

feat(protocol): `interrupt` action — stop the agent turn running on a conversation

Turns were spawned detached (`tokio::spawn` with no handle) and the canonical WS
protocol had no cancel message, so nothing could stop a turn that had gone off the
rails: a user's only recourse was to send *another* message, which spawned a second
concurrent turn racing the first. This adds the Stop button to the protocol.

`AppState` now tracks a per-conversation `CancellationToken` (`register_turn` /
`interrupt_turn` / `clear_turn` / `has_active_turn`), registered **before** the turn
is spawned so there is no window in which a turn runs but is not interruptible. The
turn task races that token against its turn future in a `tokio::select!`; cancelling
drops the turn future at its next await point — mid-LLM-stream, or between tool steps
— and the task tears down the session's confirmation/interaction parks so a late
verdict can't leak into the next turn. Registrations carry a monotonic sequence
number so a straggler turn's cleanup can never disarm the Stop button for the turn
that replaced it.

The new `interrupt` action (`spec/actions/interrupt.schema.json`) takes either a
`conversationId` or a `sessionId` — turns are keyed by conversation so a reconnect
that minted a new session can still stop the turn it is watching stream. The server
acks with `immediate_response`; the stopped turn closes itself out with a normal
`eventual_response` on **its own** `requestId`, which means clients that know nothing
about `interrupt` still resolve their streaming state correctly. `NO_ACTIVE_TURN`
when nothing is running, so a UI can distinguish "stopped it" from "nothing to stop".

Cancellation lands between the engine's await points rather than at an explicit
agent-loop checkpoint — a clean "finish the current tool, then stop" would need a
cancellation seam in the published `smooai-smooth-operator-core` crate. Nothing is
persisted for the interrupted turn, so the partial reply is not written to history.
