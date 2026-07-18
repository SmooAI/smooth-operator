---
'@smooai/smooth-operator': minor
---

Add a .NET Slack `IConnector` (`SlackConnector`) for knowledge ingestion. Resolves author names
via `users.list`, lists channels via `conversations.list`, and pages messages via
`conversations.history`. Emits one document per channel per day with a stable id
`slack:{channel}:{date}` (today re-hashes as messages land, past days dedupe on the pipeline's
(id, hash) key), `source` = the day's first-message permalink (`chat.getPermalink`), incremental
pulls via an `oldest` cursor, and a per-channel ACL label. `SourceDocument` gains an optional
`Acl` field to carry per-document access labels (mirrors the Rust `RawDocument.acl`). Threaded
replies (`conversations.replies`) are deferred to a follow-up.
