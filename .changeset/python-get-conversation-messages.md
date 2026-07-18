---
'@smooai/smooth-operator': patch
---

Python server: implement the `get_conversation_messages` action. It previously fell through to `UNSUPPORTED_ACTION`, so a web client resuming a conversation against the Python server rendered no history. The handler mirrors the merged Go/Rust reference and the `spec/actions/get-messages.schema.json` contract: newest-first `messages` (id, direction, content.text, createdAt) plus `hasMore`, with `limit` (1..100, default 50) and an optional ISO 8601 `before` cursor. `StoredMessage` gains a defaulted `created_at` timestamp to back the `createdAt` field and the cursor.
