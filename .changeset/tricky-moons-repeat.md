---
'@smooai/smooth-operator': patch
---

SECURITY (Rust server): scope conversations by owner only when an owner exists (Option B, th-909995)

`may_read_conversation` mapped every principal without an email — an anonymous connection to an
auth-enabled server, or a token carrying `sub`/`org`/`role` but no `email` — to `UserScope::Denied`
and refused it everything. The session such a connection creates is ownerless by construction, so
it was locked out of its own session: empty `list_conversations`, resume refused, `get_session` /
`get_conversation_messages` / `send_message` all `SESSION_NOT_FOUND`. The identical rule in the
.NET twin hung CI on a WebSocket ACL test and was reverted in #309.

Option B: a conversation that HAS an owner (a `user` participant with a non-blank email) is still
owner-checked, case-insensitively; one with NO owner is readable, as it was before scoping shipped.
`Denied` matches no non-empty owner, so the reported P0 stays closed: authenticated A cannot read,
resume, or `send_message` into authenticated B's owned session, and a refusal appends nothing to
B's log. `Err(_) => false` (a storage error is a denial), the `UserScope` enum, and the
`scoped_session` chokepoint at all 7 call sites are unchanged, as is the unauthenticated
`LocalServer` / smooth-daemon embedding (`UserScope::Unscoped`).

Option A (`email ?? sub`) was rejected: Go's anonymous principal uses the literal sub `"anonymous"`
for every visitor, so keying on `sub` would pool all anonymous visitors and leak their chats to
each other.

`list_conversations` now applies that same predicate per conversation instead of the
`list_conversations_by_org_and_user` storage pushdown, which cannot express "mine or ownerless" —
so the list can never disagree with what `get_session` will hand over. Still filtered before the
limit, so pages are never silently short.
