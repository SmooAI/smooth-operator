---
"@smooai/smooth-operator": patch
---

feat(ts): durable Postgres storage — sessions, conversations and the admin stores survive a restart

The TypeScript server was memory-only: every session, conversation, message and
`/admin/*` connector config, agent setting and indexing run vanished on restart.
It now has a Postgres backend, selected with `SMOOTH_AGENT_STORAGE=postgres` plus
`SMOOTH_AGENT_DATABASE_URL` (falling back to `DATABASE_URL`, but only once
`postgres` has been asked for explicitly — an ambient `DATABASE_URL` alone can
never change where data goes). Unset, or `memory`, keeps the in-memory stores
exactly as they were; an unknown value, or `postgres` with no connection string,
throws rather than silently falling back to memory, because losing durability
quietly is the failure worth shouting about.

One `PostgresStore` implements both the existing `SessionStore` and the new
`AdminStore` seam (`admin.ts`'s three maps move behind an interface;
`InMemoryAdminStore` stays the default). The schema is the Rust reference
adapter's (`rust/adapters/postgres/src/schema.rs`) copied verbatim —
`conversations`, `conversation_participants`, `conversation_messages`,
`conversation_sessions`, `connector_configs`, `agent_settings`, `indexing_runs` —
so all five servers share one set of tables rather than each inventing a dialect.
Everything is `CREATE ... IF NOT EXISTS`, so whichever server boots first creates
them. The one addition is `org_id` on `indexing_runs`, added with
`ALTER TABLE ... ADD COLUMN IF NOT EXISTS` (the same idempotent back-fill the Rust
schema uses for `knowledge_vectors.acl`), because the Rust `IndexingStore` is not
org-scoped but the `/admin/*` run list must be.

Nothing new was invented to hold TypeScript-specific state. A conversation's owner
is the email on its `user` participant row, which is what Rust's
`list_conversations_by_org_and_user` already filters on, and `contactEmail` /
`otpVerified` / `currentStepId` live in `conversation_sessions.metadata`, where
Rust already keeps `otpVerified`.

Org isolation needed a carrier, since `SessionStore` had no org anywhere.
`createSession`, `getConversation` and `listConversations` gained a trailing
`orgId` that defaults to `DEFAULT_ORG_ID` — the same `'public'` a principal
without an `org` claim already carries in `auth.ts`. That default is a specific
org, never "all orgs": widening ownership must not widen tenancy, and the
auth-disabled unscoped list is still confined to its own org. Existing callers and
the in-memory store are unaffected — the memory store accepts the argument and
ignores it, being single-tenant by construction.

Fourteen tests cover it against a real Postgres via testcontainers: a write→read
round-trip through a second connection (the durability claim itself), message
ordering and limits, resume ownership with no existence oracle, ownerless
conversations staying reachable, the scoped and unscoped conversation lists, org
isolation driven with the same email in two orgs, workflow-step and OTP-bit
persistence, and CRUD plus org isolation for all three admin stores. They skip
cleanly when Docker is unavailable; three need no Docker at all and are the guard
that the in-memory path stays the default.
