---
'@smooai/smooth-operator': minor
---

Add client-initiated turn cancellation (the "Stop button") to the TypeScript client SDK.

- `SmoothAgentClient.cancel({ requestId, sessionId? })` sends a `cancel` frame for an in-flight `send_message` turn.
- `MessageTurn.cancel()` is the ergonomic "stop THIS turn" convenience — it cancels using the turn's own `requestId` + `sessionId`.
- The terminal `cancelled` event now settles the matching `MessageTurn` as a **user-stop**: the turn *resolves* (never rejects), `await turn` yields the `Cancelled` event, the async iterator ends cleanly, and `turn.cancelled` is `true` so the UI can tell a user-stop apart from an error.
- `CancelRequest` / `Cancelled` are now first-class members of the `ClientAction` / `ServerEvent` unions (and the validator's schema maps).

Idempotent: cancelling with no active turn — or receiving a `cancelled` with no matching in-flight turn — is a harmless no-op.
