---
"@smooai/smooth-operator": patch
---

feat(go): durable Postgres storage — sessions, conversations and the admin stores survive a restart

The Go server was memory-only: every session, conversation, message and `/admin/*`
connector config, agent setting and indexing run vanished on restart. It now has a
Postgres backend, selected with `SMOOTH_AGENT_STORAGE=postgres` plus
`SMOOTH_AGENT_DATABASE_URL` (falling back to `DATABASE_URL`). Unset — or `memory` —
keeps the in-memory stores exactly as they were; an unknown value, or `postgres`
with no connection string, is a hard error rather than a silent fall back to
memory, because losing durability quietly is the failure worth shouting about.

One `PostgresStore` implements both the existing `SessionStore` and the new
`adminStore` seam. The schema is the Rust reference adapter's
(`rust/adapters/postgres/src/schema.rs`) copied verbatim — `conversations`,
`conversation_participants`, `conversation_messages`, `conversation_sessions`,
`connector_configs`, `agent_settings`, `indexing_runs` — so all five servers share
one set of tables rather than each inventing a dialect. Everything is
`CREATE ... IF NOT EXISTS`, so whichever server boots first creates them.

Nothing new was invented to store Go-specific state. A conversation's owner is the
email on its `user` participant row, which is what the Rust adapter's
`list_conversations_by_org_and_user` already filters on, and the per-session bits
(`contactEmail`, `otpVerified`, `currentStepId`) live in
`conversation_sessions.metadata`, where Rust already keeps `otpVerified`. The one
addition is an `org_id` column on `indexing_runs`, added with
`ALTER TABLE ... ADD COLUMN IF NOT EXISTS` (the same idempotent back-fill the Rust
schema uses for `knowledge_vectors.acl`): the Rust `IndexingStore` is not org-scoped
but the `/admin/*` run list must be.

Isolation is enforced in SQL, in the selection rather than after a limit.
`ConversationScope` gained an `OrgID`, carried from the authenticated principal, so
every conversation read is scoped by org first and owner second; the in-memory store
ignores it and is unchanged. A conversation in another org — like one owned by
another user — is invisible to `list_conversations` and unresumable, and reports
identically to one that never existed, so neither can be used as an existence oracle.

Thirteen tests cover it against a real Postgres via testcontainers: a write→read
round-trip through a second connection (the durability claim itself), message
ordering and limits, resume ownership with no oracle, ownerless conversations
staying reachable, the auth-disabled unscoped list, org isolation with the same email in two orgs, workflow-step and
OTP-bit persistence, and CRUD plus org isolation for all three admin stores. They
skip cleanly when Docker is unavailable, and two of them need no Docker at all —
they are the guard that the in-memory path stays the default.
