---
'@smooai/smooth-operator': minor
---

.NET server: `get_conversation_messages` pages by an opaque `cursor` (a message id) instead of the `before` ISO-timestamp cursor, and returns `nextCursor` alongside `hasMore`.

A timestamp cursor is broken by design — two messages can share a timestamp at any precision the wire keeps, so a `created_at < cursor` filter drops or repeats the collisions. An id cursor names exactly one message. The paging path no longer compares timestamps at all, and the 500-message `before` rescan window (and its paging ceiling) is gone. The .NET client SDK's `GetMessagesAction.Before` becomes `Cursor`, and `GetMessagesResult` gains `NextCursor`. Breaking wire change for clients still sending `before`.
