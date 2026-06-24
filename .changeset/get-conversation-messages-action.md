---
"@smooai/smooth-operator": minor
---

Add a `get_conversation_messages` WebSocket action to `smooth-operator-server`. Returns paginated message history for a session's conversation (`{ conversationId, messages, nextCursor, hasMore }`), wrapping the existing `StorageAdapter::list_messages_by_conversation` (the same call the admin API + turn runner use). Optional `limit` (default 50) + opaque `cursor`, newest-first. Completes wire-compat for chat clients that page history over the socket (previously only `/admin` exposed it).
