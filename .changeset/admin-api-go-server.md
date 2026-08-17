---
"@smooai/smooth-operator": patch
---

feat(go): the `/admin/*` management API, so the console works against the Go server

The management console 404s against the Go server: only Rust and C# implement the
admin API. The Go server now serves the same 14 endpoints the console's typed
client calls, on the same wire contract as
`rust/smooth-operator-server/src/admin.rs` — same paths, camelCase JSON, and the
`{"error":{"code","message"}}` envelope.

Auth matches Rust exactly: `Authorization: Bearer <token>` → verify → role-rank
gate, 401 for a missing/invalid token and 403 for an insufficient role, with
`/admin/health` ungated. Ranks are basic=0 / curator=1 / admin=2; reads are
Curator, writes are Admin. `AUTH_MODE=none` (the local dev flavor) grants Admin
exactly as Rust's `NoAuthVerifier` does — without it the console 403-walls
against a local server, which is as useless as the 404s. An auth-enabled server
is unaffected.

Conversations and messages are served from the existing session store. Connector
configs, settings and indexing runs are held in memory for now (the durable
storage adapter is a separate workstream); every read and write is org-scoped, so
one org can never see or mutate another's rows.
