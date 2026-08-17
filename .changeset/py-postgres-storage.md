---
"@smooai/smooth-operator": patch
---

feat(python): durable Postgres storage — sessions, conversations and the admin stores survive a restart

The Python server was memory-only: every session, conversation, message and
`/admin/*` connector config, agent setting and indexing run vanished on restart.
It now has a Postgres backend, selected with `SMOOTH_AGENT_STORAGE=postgres` plus
`SMOOTH_AGENT_DATABASE_URL` (falling back to `DATABASE_URL`, but only once
`postgres` has been asked for explicitly — an ambient `DATABASE_URL` alone can
never change where data goes). Unset, or `memory`, keeps the in-memory stores
exactly as they were; an unknown value, or `postgres` with no connection string,
raises rather than silently falling back to memory, because losing durability
quietly is the failure worth shouting about.

This completes the trio started in the Go (#386) and TypeScript (#392) servers.
One `PostgresStore` implements both the existing `SessionStore` and the new
`AdminStore` seam (`admin.py`'s three containers move behind an ABC;
`InMemoryAdminStore` stays the default). The schema is the Rust reference
adapter's, copied verbatim, so all five servers share one set of tables rather
than each inventing a dialect.

Nothing new was invented to hold Python-specific state. A conversation's owner is
the email on its `user` participant row, which is what Rust's
`list_conversations_by_org_and_user` already filters on; the per-conversation
workflow step lives in `conversations.metadata_json` and the per-session OTP bit
in `conversation_sessions.metadata`, where Rust already keeps `otpVerified`. The
one addition is `org_id` on `indexing_runs` via `ALTER TABLE ... ADD COLUMN IF NOT
EXISTS`, because the Rust `IndexingStore` is not org-scoped but the `/admin/*` run
list must be.

Org isolation needed a carrier, since `SessionStore` had no org anywhere.
`create_session` and `list_conversations` gained a keyword-only `org_id`
defaulting to `DEFAULT_ORG_ID` — the same `"public"` a principal without an `org`
claim already carries in `auth.py`. That default is a specific org, never "all
orgs": the auth-disabled unscoped list is still confined to its own org, because
widening ownership must not widen tenancy. Existing callers and the in-memory
store are unaffected — the memory store accepts the argument and ignores it, being
single-tenant by construction.

`asyncpg` is an optional dependency (the `postgres` extra); nothing imports
`postgres_store` unless the env var selects it, so the memory path needs no
database driver installed.

Fourteen tests cover it against a real Postgres via testcontainers: a write→read
round-trip through a second connection (the durability claim itself), message
ordering and limits, resume ownership with no existence oracle, ownerless
conversations staying reachable, the scoped and unscoped conversation lists, org
isolation driven with the same email in two orgs, workflow-step and OTP-bit
persistence (including clearing each without disturbing the other keys), and CRUD
plus org isolation for all three admin stores. They skip cleanly when Docker is
unavailable; three need no Docker at all and are the guard that the in-memory path
stays the default.
