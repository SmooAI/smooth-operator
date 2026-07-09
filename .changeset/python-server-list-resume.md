---
"@smooai/smooth-operator": patch
---

Python server: `list_conversations` + resume-by-`conversationId` (pearl th-d5b446) — Python parity with the merged Rust/Go/TS reference so every client (daemon PWA, `th code` TUI, chat-widget) can build a conversation sidebar + resume against the Python server too.

- New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200, "Conversations") with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message with leading markdown/control chars stripped, falling back to a generic name; `updatedAt` = ISO-8601.
- `create_conversation_session` gains an optional `conversationId`: when it names a known conversation, the new session RESUMES — reuses that conversation's id and keeps its message log, so `send_message` appends to it and the runner replays its history. Absent/unknown id ⇒ a fresh conversation is minted (unchanged behavior).
- Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `SessionStore` gains `list_conversations()` + an optional `conversation_id` arg on `create_session`; the in-memory store tracks per-conversation last-activity for the sort key.
