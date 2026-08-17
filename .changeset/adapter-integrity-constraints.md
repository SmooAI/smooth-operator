---
'@smooai/smooth-operator': patch
---

Push the Postgres adapter's data integrity into the schema.

The json columns (`metadata_json`, `analytics_json`, `metadata`) are now `NOT NULL DEFAULT
'{}'`, so "absent" has one representation on read instead of two. `platform` and session
`status` gain CHECK constraints alongside the ones `direction` and participant `type` already
had, and the timestamps that were fully nullable on `conversation_sessions` are now
`NOT NULL DEFAULT now()`.

A bare DEFAULT was not enough: the inserts pass an explicit NULL for an absent optional, and
a DEFAULT only fires on an omitted column, so the inserts coalesce in SQL.

Rust only — the Go, TypeScript, Python and .NET stores keep their own copies of this DDL and
are being brought across separately against this as the reference.
