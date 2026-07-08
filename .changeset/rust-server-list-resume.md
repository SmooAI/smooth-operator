---
'@smooai/smooth-operator': patch
---

Rust server: conversation-history / resume substrate for the WS protocol (pearl th-d5b446) — the contract every client (daemon PWA, `th code` TUI, chat-widget) builds a conversation sidebar + resume against.

- New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200) with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message, falling back to the conversation `name`; `updatedAt` = ISO-8601.
- `create_conversation_session` gains an optional `conversationId`: when it names an existing conversation, the new session RESUMES — reuses that conversation's id + org and skips `create_conversation`, so `send_message` appends to it and the runner replays its history via `thread_id`. Absent/unknown id ⇒ a fresh conversation is minted (byte-for-byte unchanged behavior).
- Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `handler.rs` only.
