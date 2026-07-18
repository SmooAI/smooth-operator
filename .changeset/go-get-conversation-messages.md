---
'@smooai/smooth-operator': patch
---

Go server: implement the `get_conversation_messages` action. It previously fell through to `UNSUPPORTED_ACTION`, so a web client resuming a conversation against the Go server rendered no history. The handler mirrors the Rust reference and the `spec/actions/get-messages.schema.json` contract: newest-first `messages` (id, direction, content.text, createdAt) plus `hasMore`, with `limit` (1..100, default 50) and an optional ISO 8601 `before` cursor. `StoredMessage` gains a `CreatedAt` timestamp to back the `createdAt` field and the cursor.
