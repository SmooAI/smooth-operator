---
"@smooai/smooth-operator": patch
---

feat(go): client-initiated turn cancellation — Client.Cancel() + MessageTurn.Cancel()

The wire protocol and all five servers already honor the `cancel` frame / `cancelled`
event, but the Go client had no way to send it. Add the missing client surface, mirroring
the TypeScript reference (#459):

- `Client.Cancel(CancelParams{RequestID, SessionID?})` sends the `cancel` frame.
- `MessageTurn.Cancel()` is the ergonomic "stop THIS turn" convenience — it cancels using
  the turn's own requestId + originating sessionId. Fire-and-forget, idempotent.
- A terminal `cancelled` event settles the matching turn as a user-stop: the event is
  delivered on `Events()`, the channel closes cleanly, `Wait` resolves WITHOUT an error,
  and `MessageTurn.Cancelled()` reports `true` so a UI tells a user-stop apart from a
  failure. Errors still go the error path.
- `cancel` is now first-class in the `ActionType` set and `cancelled` in the `EventType`
  set (with `ServerEvent.AsCancelled()`), not stringly-typed.
- Idempotent: a cancel with no active turn, or a `cancelled` with no matching turn, is a
  harmless no-op.
