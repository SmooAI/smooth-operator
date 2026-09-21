---
'@smooai/smooth-operator': patch
---

Python server: conversation-history / resume substrate for the WS protocol (pearl th-d5b446) — Python parity with the merged Rust reference so every client (daemon PWA, `th code` TUI, chat-widget) can build a conversation sidebar + resume against the Python server too.

- New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200, message "Conversations") with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message with leading markdown/control chars stripped, falling back to the conversation `name`; `updatedAt` = ISO-8601.
- `create_conversation_session` gains an optional `conversationId`: when it names a known conversation, the new session RESUMES — reuses that conversation's id and keeps its message log, so `send_message` appends to it and the runner replays its history. Absent/unknown id ⇒ a fresh conversation is minted (byte-for-byte unchanged behavior).
- Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `python/server/src/smooth_operator_server/{dispatcher,session_store}.py` only. The in-memory store now tracks a per-conversation `updated_at` (bumped on each append) for the sort key.
