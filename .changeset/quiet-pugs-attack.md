---
'@smooai/smooth-operator': patch
---

SECURITY: TS server — owner-check only conversations that HAVE an owner (Option B)

The per-user scoping rule shipped in #297 scoped an authenticated principal with no
`email` claim (and an anonymous connection to an auth-enabled server) to an unownable
sentinel, and required `ownerEmail === scope` on every read. That denied such callers
EVERYTHING — empty list, resume refused, `send_message` refused — locking them out of
the session they had just created, i.e. no anonymous or emailless chat at all on an
auth-enabled server. The identical rule in .NET hung CI on a WebSocket ACL test and was
reverted in #309; TS had no equivalent test, so it went unnoticed here.

`mayRead` now allows a conversation with NO owner and owner-checks one that has an
owner. The reported P0 stays closed: authenticated A still cannot read or write
authenticated B's owned session (`SESSION_NOT_FOUND`, byte-identical to a never-existed
id, nothing appended to B's log), and an emailless scope still matches no real owner —
the list stays empty for emailless principals rather than pooling every anonymous
visitor's chats into one readable bucket.

Owner comparison is now case-insensitive (read path and list selection), matching .NET
and Python — OIDC providers vary on the casing they emit for the same identity.
