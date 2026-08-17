---
"@smooai/smooth-operator": patch
---

fix(ts,python): scope conversations by organization, not just owner

The same gap Go closed in #405. The ownership gate consulted the owner only, and
because an **ownerless** conversation is deliberately reachable (it keeps
anonymous, emailless-authenticated and legacy sessions usable), an ownerless
conversation belonging to **another org** was readable by anyone holding its id —
authorization resting on an unguessable UUID.

Org is now the OUTER scope, checked before ownership: an auth-disabled server is
still fully unscoped, a conversation from another org is invisible regardless of
ownership, and ownerless conversations stay reachable **within their own org**.
The owning org is recorded on the conversation at creation, never rewritten on
resume, and carried on the stored session so the dispatcher chokepoint can check
it. A conversation with no org recorded falls through to the ownership check, so
rows predating org capture stay reachable by their owners.
