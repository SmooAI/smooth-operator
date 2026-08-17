---
"@smooai/smooth-operator": patch
---

fix(go): scope conversations by organization, not just owner

`ConversationScope.Allows` consulted ownership only. Because an **ownerless**
conversation is deliberately reachable (th-909995 Option B keeps anonymous,
emailless-authenticated and legacy sessions usable), an ownerless conversation
belonging to **another org** was readable by anyone holding its conversation id —
authorization resting on an unguessable UUID, which leaks through logs, referrers
and screenshots.

Org is now the OUTER scope, checked before ownership: `Unscoped` (auth-disabled
dev) still short-circuits first, a conversation from another org is invisible
regardless of ownership, and ownerless conversations stay reachable **within
their own org** — so th-909995 is preserved rather than reverted. The owning org
is recorded on the conversation at creation and carried on `StoredSession` so the
dispatcher chokepoint can check it.

A conversation with **no** org recorded falls through to the ownership check, so
rows created before org capture are not locked away from the people who own them.

TypeScript and Python have the same gap and follow in their own PRs.
