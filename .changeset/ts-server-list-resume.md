---
'@smooai/smooth-operator-server': minor
---

TS server: `list_conversations` + resume-by-`conversationId`

Mirror the merged Rust reference (pearl th-d5b446) on the TypeScript
smooth-operator-server — the conversation-sidebar / resume substrate every client
builds against.

- New WS action `list_conversations`: most-recent-first, only conversations with
  `messageCount > 0` (drops empty-on-page-load spam), each with a first-inbound
  title preview (~60 chars, leading markdown/control chars stripped, name
  fallback), ISO-8601 `updatedAt`, and `messageCount`. Optional `limit` (default 50).
- `create_conversation_session` gains optional `conversationId`: when it names an
  existing conversation, resume — reuse its id + persisted message log, so
  `send_message` appends and the runner replays history. Absent/unknown id keeps
  minting fresh (unchanged).

Additive + back-compat: no `conversationId` / no `list_conversations` call =
unchanged behavior. New tests cover list filter/preview/order/limit, resume
binding + history replay, and unknown-id fallback.
