---
"@smooai/smooth-operator": minor
---

Add client-initiated turn cancellation (the "Stop button") to the Python async client SDK, mirroring the TypeScript client.

- `SmoothAgentClient.cancel(request_id=..., session_id=None)` sends a `cancel` frame for an in-flight `send_message` turn.
- `MessageTurn.cancel()` is the ergonomic "stop THIS turn" convenience — it cancels using the turn's own `request_id` + originating `session_id`.
- The terminal `cancelled` event now settles the matching `MessageTurn` as a **user-stop**: the turn *resolves* (never raises), `await turn` yields the `Cancelled` event, the async iterator ends cleanly, and `turn.cancelled` is `True` so callers can tell a user-stop apart from an error.
- `CancelRequest` / `Cancelled` are now first-class members of the `ClientAction` / `ServerEvent` unions (and the validator's schema maps).

Idempotent: cancelling with no active turn — or receiving a `cancelled` with no matching in-flight turn — is a harmless no-op.
