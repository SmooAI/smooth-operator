---
"@smooai/smooth-operator-server": patch
---

fix(ts): cancel discards a HITL-parked confirmation so the parked turn drops cleanly

Cancelling a turn parked at a write-confirmation (HITL) freed the slot and emitted `cancelled`
correctly, but the park in `turnRunner.ts` awaits a bare deferred from `ConfirmationRegistry.register`
(`const approved = await verdict`) that the turn's `cancelSignal` abort does NOT itself complete. The
cancel path already discarded it via a connection-wide `confirmations.rejectAll()`, but that is a
broader sweep than the cancel needs.

`FrameDispatcher.cancelActiveTurn` now discards precisely the cancelled turn's pending confirmation
(`confirmations.resolve(turn.sessionId, false)`) after aborting the controller, so the parked await
unblocks immediately (resolves denied; the result is dropped because the sink is gagged and the slot
is already cleared). `activeTurn` now carries a `sessionId`, stamped where the turn is created in the
`send_message` handler. The disconnect path still rejects every outstanding confirmation separately
via `rejectPendingConfirmations`, so nothing dangles there. Mirrors the Rust reference dropping the
confirmation future on abort and the .NET fix (#460). No behavior change for a non-parked cancel or
the no-active-turn no-op.

Adds a parity test (mirroring the .NET `CancelUnparkTests`) that drives a turn to
`write_confirmation_required`, cancels it, and asserts `cancelled` is emitted, a later
`confirm_tool_action` returns `NO_PENDING_CONFIRMATION`, the slot is freed (a new `send_message` is
accepted), and no stray events leak from the abandoned turn.
