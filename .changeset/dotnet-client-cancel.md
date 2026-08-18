---
"@smooai/smooth-operator": patch
---

feat(dotnet): client-initiated turn cancellation — SmoothAgentClient.Cancel() + MessageTurn.Cancel()

The wire protocol and all five servers already honor the `cancel` frame / `cancelled` event,
but the .NET client had no way to send it — the only stop was `MessageTurn.Abort()`, a local
force-close that faults the turn and never hits the wire.

Add the missing client surface, mirroring the TypeScript SDK (#459):

- `SmoothAgentClient.Cancel(requestId, sessionId?)` sends the `cancel` frame (fire-and-forget).
- `MessageTurn.Cancel()` is the ergonomic "stop THIS turn" convenience, carrying the turn's own
  requestId + originating sessionId.
- A terminal `cancelled` event settles the matching turn as a user-stop: it **resolves** (never
  faults) — the async iterator ends cleanly after yielding the terminal `CancelledEvent`,
  `MessageTurn.Completion` yields `null`, and `MessageTurn.Cancelled` is `true` (with
  `CancelledEvent` carrying status 499). Errors still throw, so a UI can tell a stop from a failure.
- `CancelAction` / `CancelledEvent` are now first-class in the `ClientAction` / `ServerEvent`
  unions and the `ProtocolValidator` schema maps.
- Idempotent: a `Cancel` with no active turn, or a `cancelled` with no matching turn, is a
  harmless no-op that does not throw.

`MessageTurn.Completion` is now `Task<EventualResponseEvent?>` (null on a user-stop). The
`IChatClient` facade contract is unchanged — its `GetResponseAsync` surfaces a mid-flight native
cancel as `OperationCanceledException`, the idiomatic MEAI "generation stopped" signal.
