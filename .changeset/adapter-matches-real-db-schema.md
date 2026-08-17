---
'@smooai/smooth-operator': minor
---

Make the Postgres adapter's schema apply against the real smooai database.

`schema.rs` claimed to mirror the monorepo and did not. `conversation_messages` declared
`from_ref`/`to_ref` JSONB where the monorepo has `from`/`to` participant FK columns, and a
`seq BIGSERIAL` the monorepo has never had — so `CREATE INDEX ... (conversation_id, seq)`
aborted schema init with `column "seq" does not exist`, and every server applying that schema
failed at boot against the real database.

The adapter now uses the real column names, stores participant ids rather than a denormalized
JSON blob (a `ParticipantRef`'s type and name come from joining `conversation_participants`),
and pages on `(created_at, id)` instead of a `seq` counter — a stable total order that needs no
extra column. The module doc now lists what still differs instead of claiming parity.
