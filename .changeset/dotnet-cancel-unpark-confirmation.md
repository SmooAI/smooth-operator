---
"@smooai/smooth-operator": patch
---

fix(dotnet): cancel discards a HITL-parked confirmation so the parked turn drops cleanly

Cancelling a turn parked at a write-confirmation (HITL) freed the slot and emitted `cancelled`
correctly, but left the parked `Task` lingering: the park awaits a bare `TaskCompletionSource<bool>`
from `ConfirmationRegistry.Register` that is NOT linked to the per-turn cancellation token, so
cancelling the CTS never completed that await. The parked task stayed alive (silently gagged by the
`Cancelled` flag) until the next `Register`/disconnect evicted its pending confirmation.

`FrameDispatcher.TryCancelActiveTurn` now discards the cancelled turn's pending confirmation
(`_confirmations.Resolve(turn.SessionId, approved: false)`) after cancelling the CTS, so the parked
await unblocks immediately (resolves denied; the result is dropped because the sink is gagged and
`_turn` is already null). To reach the session id from the cancel path, `ActiveTurn` now carries a
`SessionId`, stamped where the turn is created in `HandleSendMessageAsync`. Mirrors the Rust
reference dropping the confirmation future on `handle.abort()`. No behavior change for a non-parked
cancel or the no-active-turn no-op. An xUnit parity test drives a turn to `write_confirmation_required`,
cancels it, and asserts `cancelled` is emitted, a later `confirm_tool_action` returns
`NO_PENDING_CONFIRMATION`, the slot is freed, and no stray events leak from the abandoned turn.
