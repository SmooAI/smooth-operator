---
'@smooai/smooth-operator': minor
---

SMOODEV-3744: `list_conversations` pages with a keyset cursor and searches on the server, in all five servers and clients.

The history sidebar could only ever return the newest `limit` conversations, and a client-side search could only filter what it had loaded, so older chats were unreachable. `list_conversations` now takes an optional opaque `cursor` (a prior reply's `nextCursor`) and an optional `query`, and replies with `nextCursor` + `hasMore` beside `conversations`.

- Paging is keyset on `(updatedAt, conversationId)`, never offset: pages never repeat a row, and an untouched row is never skipped. A row updated mid-paging jumps above the cursor and heads a fresh first page instead.
- `query` keeps rows whose meaningful name or first inbound message contains the text, case-insensitively, applied inside the caller's org + user scope, so it can never surface another member's or an ownerless conversation.
- No `cursor` and no `query` is the original first page, so existing clients are unchanged. The action now has a schema (`spec/actions/list-conversations.schema.json`) and an `ActionType` in every client.

Rust: new `StorageAdapter::list_owned_conversation_summaries_page(org, email, &ConversationSummaryQuery)` with a correct default (backends override it with one keyset query); `list_owned_conversation_summaries` now delegates to it. Adds `ConversationKey` (cursor encode/decode), `ConversationSummaryQuery`, `sort_conversations_newest_first` and `DEFAULT_CONVERSATION_NAME_PREFIX`.
