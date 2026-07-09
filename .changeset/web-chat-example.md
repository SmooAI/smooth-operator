---
'@smooai/smooth-operator': patch
---

SDK: `SmoothAgentClient.listConversations()` + `conversationId` resume typing — the client surface for a conversation sidebar (pearl th-2f028f).

- New `listConversations({ limit? })` method wrapping the server's `list_conversations` action; resolves to `{ conversations: [{ conversationId, title, updatedAt, messageCount }] }` (most-recent-first). Exports `ConversationSummary` / `ListConversationsResponse`.
- `createConversationSession` now accepts an optional `conversationId` (already honored by the server) to RESUME an existing conversation; pair it with `getMessages` to load the transcript.
- Additive and back-compat.

Also adds `examples/web-chat` — a private, runnable Vite + React reference chat client built on this SDK (token streaming, inline tool-call/result blocks, HITL approvals, conversation sidebar, oldest-first history). Not published.
