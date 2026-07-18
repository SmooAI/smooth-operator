---
'@smooai/smooth-operator': minor
---

Python server: `get_conversation_messages` pages by opaque `cursor`, not the `before` timestamp.

The handler now reads `cursor` (a message id today), locates that message in the conversation log, and returns the page immediately older than it. Responses carry `nextCursor` — the id of the oldest message in the page, non-null exactly when `hasMore` is true. An unknown or stale cursor is a `VALIDATION_ERROR` rather than a silently empty page. `createdAt` stays on every message for display, with microsecond precision intact; it is simply no longer the cursor.

This removes code. The old `_BEFORE_SCAN_WINDOW = 500` bounded rescan existed only because a timestamp cannot locate a position in the log — it capped `before` paging to the newest 500 messages. An id cursor locates the position exactly, so the window, the ISO parsing (`_parse_before`), and the `created_at <` comparison are all gone. There is no timestamp comparison left on the paging path.

Matches the spec change in #279 and the Rust reference. Tests cover round-trip paging to exhaustion, the identical-`created_at` collision case a timestamp cursor provably cannot survive (the bug the Go server shipped), and the unknown-cursor error.
