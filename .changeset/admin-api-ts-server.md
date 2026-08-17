---
"@smooai/smooth-operator": patch
---

feat(ts): the `/admin/*` management API, so the console works against the TypeScript server

The management console 404s against the TypeScript server: only Rust and C#
implement the admin API. The TS server now serves the same 14 endpoints the
console's typed client calls, on the same wire contract as
`rust/smooth-operator-server/src/admin.rs` — same paths, camelCase JSON, and the
`{"error":{"code","message"}}` envelope. Plain HTTP previously answered every
request with a 426; `/admin/*` is now handled and everything else still is.

Auth matches Rust: Bearer token → verify → role-rank gate, 401 for a missing or
invalid token and 403 for an insufficient role, `/admin/health` ungated. Reads
are Curator, writes Admin. `AUTH_MODE=none` (the local dev flavor) grants Admin
exactly as Rust's `NoAuthVerifier` does — without it the console 403-walls
against a local server. An auth-enabled server is unaffected.

Conversations and messages come from the existing session store; connector
configs, settings and indexing runs are in memory for now (the durable storage
adapter is a separate workstream) and every row is org-scoped.
