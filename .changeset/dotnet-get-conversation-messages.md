---
'@smooai/smooth-operator': patch
---

.NET server: implement the `get_conversation_messages` action

The .NET `FrameDispatcher` answered `UNSUPPORTED_ACTION` for
`get_conversation_messages`, so a C#-hosted server couldn't page conversation
history the way the Rust/Go/TS servers can — a client resuming a conversation
had no way to load prior messages. It now returns `{messages, hasMore}`
newest-first per `spec/actions/get-messages.schema.json`, with `limit` (1–100,
default 50) and an optional ISO 8601 `before` cursor.

`StoredMessage` gained a `CreatedAt` init-only property (not a positional
parameter, so downstream `ISessionStore` implementations keep compiling) that
the Postgres store now reads from — and returns on append via — the existing
`conversation_messages.created_at` column.
