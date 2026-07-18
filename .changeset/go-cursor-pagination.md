---
'@smooai/smooth-operator': minor
---

Go server: page `get_conversation_messages` by opaque id cursor instead of an ISO timestamp.

The request field `before` (ISO 8601) is replaced by `cursor` (opaque, a message id today), and the response now carries `nextCursor` — the id of the oldest message in the page, non-null exactly when `hasMore` is true. Breaking wire change.

This removes the failure mode rather than renaming it: a timestamp cursor cannot separate two messages that share a timestamp, so `created_at <` paging silently dropped every message colliding on the cursor's instant. An id names exactly one row. The paging path now contains no timestamp comparison, and the bounded 500-message rescan the timestamp cursor required is gone — an id cursor locates its position in the log directly, so paging has no depth ceiling. `createdAt` is still returned (RFC3339Nano) for display.

An unknown or stale cursor now returns a `VALIDATION_ERROR` instead of a silent empty page.
