---
'@smooai/smooth-operator': patch
---

SECURITY (Go): fix conversation ownership check to close cross-user access without locking out anonymous and emailless principals.

The previous rule (`s.Unscoped || (s.Email != "" && ownerEmail == s.Email)`) denied everything to any connection whose principal carried no email claim. On an auth-enabled server that population is real — `AnonymousPrincipal` has no email, and plenty of IdPs issue tokens without one — so those callers got an empty conversation list, a refused resume, and a refused `send_message` on the session they had just created. That is an outage for anonymous/public-agent chat, a supported scenario. The identical rule in the .NET sibling hung CI on a WebSocket ACL test that authenticates without an email claim, forcing a revert there (#309); Go had no equivalent test, which is why it went unnoticed.

`ConversationScope.Allows` now owner-checks only conversations that HAVE an owner:

- a session with an owner is readable and writable only by that same principal — this keeps the reported P0 (authenticated A reaching into authenticated B's owned session) closed on both the read and the write path;
- a session with no owner (anonymous, emailless-authenticated, or legacy auth-disabled) stays reachable, since there is no owner to enforce on behalf of.

Keying ownerless sessions on `sub` instead was considered and rejected: `AnonymousPrincipal.Sub` is the literal string `"anonymous"` for every visitor, so it would pool all anonymous conversations into one shared bucket and leak them to each other.

Email comparison is now case-insensitive (`strings.EqualFold`), matching the .NET and Python siblings — OIDC providers vary on the casing of the email claim.

Unchanged: the `scopedSession` chokepoint every sessionId-taking handler routes through, the identical `SESSION_NOT_FOUND` for not-yours vs never-existed (no existence oracle), selection-side filtering for the conversation list, and the auth-disabled unscoped path.
