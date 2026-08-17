---
'@smooai/smooth-operator': minor
---

Converge the C# server's Postgres store onto the shared schema.

The C# server had invented its own tables: `conversation_identity_state` and
`conversation_workflow_state` side tables, a narrower `conversation_sessions` carrying its own
`user_email` column, and no `conversations` or `conversation_participants` at all. It now reads and
writes the same shape as the Rust source of truth (`rust/adapters/postgres/src/schema.rs`) and the
Go store, so one database can be driven by any of the servers.

The per-session bits the side tables held moved into `conversation_sessions.metadata` under the key
names the other servers already read (`contactEmail`, `otpVerified`, `currentStepId`), and the
conversation owner now lives on the `user` participant rather than a duplicated session column — so
a resumed session reports the original owner instead of whoever resumed it.

A legacy database migrates in place on store init: the tables are widened, the side tables and
`user_email` are backfilled into `metadata` and `conversation_participants`, and the invented tables
are dropped. The store's `ISessionStore` surface is unchanged.
