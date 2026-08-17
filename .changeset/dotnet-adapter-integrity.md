---
"@smooai/smooth-operator": patch
---

feat(dotnet): let the Postgres schema enforce what the store was only hoping (th-5a5181 P2)

The .NET slice of the adapter-integrity wave, mirroring the Rust reference (#425):
`metadata_json` / `analytics_json` / `metadata` become `JSONB NOT NULL DEFAULT '{}'`
so "absent" has one representation on read instead of two; `platform` and session
`status` gain CHECK constraints alongside the ones `direction` and participant `type`
already had (status stays optional — a NULL passes a CHECK); and
`conversation_sessions.created_at` / `updated_at` / `last_activity_at` become
`NOT NULL DEFAULT now()` like every other table's.

Two places this slice differs from the Rust one, both deliberate:

**No `coalesce` in the INSERTs.** Rust needed it because its inserts pass an explicit
NULL for an absent `Option`, and a bare `DEFAULT` only fires on an OMITTED column. This
store omits those columns entirely, so the DEFAULT already fires — a coalesce here would
be dead SQL. The only json column it writes explicitly is
`conversation_sessions.metadata`, which is always a serialized object.

**`platform` was `'smooth-operator'`, which the new CHECK rejects.** That is the product
name, not a channel, and it is not in the shared platform vocabulary — applying the CHECK
without fixing it would have failed every conversation insert. Now `'web'`, matching the
Go store, which is what a browser WebSocket chat is.

The migration block also closes the constraints on a LEGACY database: `CREATE TABLE IF
NOT EXISTS` is a no-op there, so the DDL's NOT NULLs would never have applied. It
backfills the nulls then `SET NOT NULL` / `SET DEFAULT`, so a migrated database ends up
with the same guarantees as a fresh one. This is the only server with a migration block,
so it is the only one that can. The CHECKs are deliberately NOT retrofitted — a legacy
row's platform is `'smooth-operator'`, so adding that constraint would fail init on
exactly the databases that need migrating.

Tests: a session created with no metadata reads back `{}` rather than null across
conversations, messages and participants; a migrated legacy database reports its columns
as NOT NULL; and the CHECKs reject `'smooth-operator'` and an unknown session status.
70 green against real pgvector containers.
