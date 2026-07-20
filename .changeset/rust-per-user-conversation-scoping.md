---
'@smooai/smooth-operator': minor
---

**SECURITY (Rust): per-user conversation scoping — fixes a cross-user data leak.**
`list_conversations` was scoped by organization only, and the resume-by-`conversationId`
path plus `get_conversation_messages` were not owner-checked at all, so any authenticated
user in an org could enumerate and open every other user's conversations in that org.

Conversation reads are now scoped to the connection's **authenticated principal** (the
JWT `email` claim, surfaced as `Principal::email`), on top of — never instead of — the
existing org scope. The scope is derived only from the verified token: a create frame's
client-supplied `userEmail` no longer decides the session's identity when the connection
is authenticated (that was the spoofing vector), and the same fix is applied to the
Lambda transport's create path. `StorageAdapter` gains
`list_conversations_by_org_and_user`, which filters in the query rather than after a
limit; Postgres pushes it down to one `EXISTS` query, other adapters use the trait's
participant-filtering default, so a new adapter is scoped by construction.

Fail-closed rules: auth enabled + principal email ⇒ scoped; auth enabled but no principal
or no `email` claim ⇒ empty list and every read denied (never a silent fall back to the
whole org); auth **disabled** (`AUTH_MODE=none`, unconfigured, or the single-user
`local-token` daemon / `LocalServer`) ⇒ unscoped, behavior unchanged. Denials are
indistinguishable from genuine misses — another user's session returns the byte-identical
`SESSION_NOT_FOUND` an unknown id returns, and resuming another user's conversation mints
a fresh one exactly as an unknown id does, so there is no existence oracle to enumerate
conversation ids with.
