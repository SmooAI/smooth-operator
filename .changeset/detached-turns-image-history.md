---
'@smooai/smooth-operator': minor
---

SMOODEV-3705 / SMOODEV-3706: Turns survive a client disconnect, and photos survive into history.

- **A client disconnect no longer aborts the running turn** (Rust server). The turn runs to completion and persists its reply; a turn that fails persists an outbound message marked `metadataJson.turnError = {code, requestId}`, so a client that dropped mid-turn (a phone backgrounded behind a relay) finds the outcome in the history on reconnect. The disconnect is logged at `info`. Only `cancel` stops a turn.
- **One turn per conversation**, not per connection: a `send_message` on a conversation whose turn is still running — from this socket or an earlier one — is rejected with `TURN_IN_PROGRESS`. A `cancel` carrying a `sessionId` on a connection with no turn of its own now cancels that conversation's orphaned turn.
- **Image-only sends**: `send_message.message` may be empty when the turn carries an `images[]` or `files[]` attachment (spec, Rust, Python; the TS/Go/.NET servers already accepted it). The Python server also stops sending an empty text part ahead of the image.
- **History replay keeps images** (Rust): the newest 3 image-bearing user messages replay their images to the model, image-only messages are no longer skipped, and older images become an `[image omitted]` note. Turn-error records are not replayed.
