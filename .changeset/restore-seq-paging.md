---
'@smooai/smooth-operator': patch
---

Restore `seq` paging in the Postgres adapter, and stop calling it a mirror of the monorepo.

The previous release dropped `seq` from the Rust adapter to make its schema apply against the
smooai monorepo database. That goal is closed: the deployed operator persists through a
separate private adapter over the real tables (ADR-041), so this crate is the OSS operator's
own STANDALONE store and the two schemas are allowed to differ.

Dropping `seq` therefore bought nothing and left Rust paging on `(created_at, id)` while the
Go, TypeScript, Python and .NET stores still paged on `seq`. `seq` is back — a
database-assigned counter cannot tie, which makes it a stronger paging key — and all five
implementations agree again.

The `from`/`to` participant-id columns and their join are kept: de-denormalizing a
`ParticipantRef` blob that could carry a stale name and type is an integrity win independent
of which database this points at.
