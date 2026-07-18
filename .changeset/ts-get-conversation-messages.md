---
'@smooai/smooth-operator': patch
---

TypeScript server: implement the `get_conversation_messages` action. Its dispatcher switch stopped at `verify_otp`, so the action fell through to `UNSUPPORTED_ACTION` and a web client resuming a conversation against the TS server rendered no history. The handler mirrors the merged Go/Rust references and the `spec/actions/get-messages.schema.json` contract: newest-first `messages` (id, direction, content.text, createdAt) plus `hasMore`, with `limit` (1..100, default 50) and an optional ISO 8601 `before` cursor. `StoredMessage` gains an optional `createdAt` timestamp (set by `InMemorySessionStore.appendMessage`) to back the `createdAt` field and the cursor.
