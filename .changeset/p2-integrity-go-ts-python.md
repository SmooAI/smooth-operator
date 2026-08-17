---
"@smooai/smooth-operator": patch
---

feat(go,ts,python): mirror the P2 schema integrity constraints into the three servers

Mirrors core#425 (Rust adapter, `459b4b4`) into `go/server/postgres_store.go`,
`typescript/server/src/postgresStore.ts` and
`python/.../postgres_store.py` — identical DDL in all three:

- `metadata_json` / `analytics_json` / `metadata` → `JSONB NOT NULL DEFAULT '{}'::jsonb`
  on conversations, participants, messages and sessions, so "absent" has ONE
  representation on read instead of two.
- `platform` gains a `CHECK` over the ten known values; `status` gains a `CHECK`
  over `active` / `idle` / `ended`. Status stays optional — NULL passes a CHECK, so
  the value is constrained without the column becoming required.
- `conversation_sessions.created_at` / `updated_at` / `last_activity_at` →
  `NOT NULL DEFAULT now()`.

**No `coalesce()` was needed in any of the three, unlike Rust.** The Rust adapter
names every column in its INSERTs and passes an explicit NULL for an absent
`Option`, and `DEFAULT` only fires on an *omitted* column — hence its four
coalesces. These three servers **omit** the json columns from every
conversation-domain INSERT, so the DEFAULT fires on its own. The one JSONB they do
pass explicitly (`conversation_sessions.metadata`) is always a serialized object,
never NULL, in all three. Verified against real Postgres containers rather than
assumed.

Also skipped, per the Rust PR: the `(organization_id, browser_fingerprint)` index.
No server here queries that column.

Two tests per language, run against real containers: absent json reads back as `{}`
(the one that fails if either the `NOT NULL DEFAULT` or the omit-from-INSERT
regresses), and an unknown platform is rejected by the CHECK.

Green: Go `vet` + full `go test` (14 postgres tests), TypeScript `tsc` + 312 tests,
Python ruff + 318 tests. All exit 0.

Note carried over from the Rust PR: the DDL is `CREATE TABLE IF NOT EXISTS`, so
these constraints apply to newly-created databases. An existing table is unchanged
until someone writes a migration — same as Rust, and worth a follow-up rather than
a silent assumption.
