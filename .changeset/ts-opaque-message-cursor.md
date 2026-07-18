---
'@smooai/smooth-operator': minor
---

TypeScript server: `get_conversation_messages` now pages on an opaque `cursor` (a message id) instead of the `before` ISO-timestamp cursor, and returns `nextCursor` alongside `hasMore`.

Breaking wire change. A timestamp cursor cannot page a log correctly — two messages can share a `createdAt` at any precision the wire keeps, so a `createdAt < cursor` filter drops or repeats the messages that collide. The cursor now names exactly one message: the page starts immediately after it, on the older side. `nextCursor` is the oldest message in the page, non-null exactly when `hasMore` is true; an unknown cursor is a `VALIDATION_ERROR` rather than a silent empty page.

This also removes the 500-message bounded rescan the timestamp cursor required, so paging is no longer capped at the newest 500 messages. `createdAt` stays on every message for display.
