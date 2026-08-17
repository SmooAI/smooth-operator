---
"@smooai/smooth-operator": patch
---

fix: the Postgres stores must report the conversation's org, or the new org gate silently falls open

PRs #405/#408 made organization the OUTER scope on conversation access, but only
wired the in-memory stores and the dispatcher gate. The Postgres stores kept
returning sessions with the org field unset, and the gate treats an unrecorded
org as "fall through to ownership" — deliberately, so rows predating org capture
don't lock their owners out. The result: on the one backend that actually holds
several organizations' data, a cross-org read of an ownerless conversation was
allowed again, given only a session id.

It compiled, and every test passed: the Postgres store tests never go through the
gate, and the gate tests never use a Postgres store. Nothing failed loudly.

Go, TypeScript and Python now read `organization_id` off the session row (and, in
TS, off `getConversation`) and stamp it on the returned session. The data was
already being persisted correctly — only the read path dropped it — so this is a
read fix, with no schema change and no migration.

One regression test per language, each using an OWNERLESS conversation on purpose:
with an owned one, ownership alone blocks the cross-org read and the test passes
without proving anything about the org check. The Go test drives the real
`ConversationScope.Allows` rather than just asserting the field, and is
mutation-verified — reverting the fix fails it with `OwnerOrg = ""`.
