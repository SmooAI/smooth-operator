---
'@smooai/smooth-operator': patch
---

Go server: implement turn cancellation — the `cancel` action (the "Stop button"), porting the Rust reference. A connection now runs at most ONE agent turn at a time: `send_message` registers its turn with a cancellable context, a `cancel` frame cancels it and emits the terminal `cancelled` event (`status: 499`, echoing the cancelled turn's `requestId` at the envelope level and inside `data`), and a second `send_message` while a turn is in flight is rejected with `TURN_IN_PROGRESS` rather than run concurrently. A cancel with no active turn is a silent no-op. A cancelled turn discards its partial assistant message (never persisted) and emits no `eventual_response`; the user's message stays persisted. A client disconnect mid-turn aborts the turn the same way, while the SIGTERM graceful-drain path still lets an in-flight turn finish.
