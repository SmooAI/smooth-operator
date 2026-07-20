---
'@smooai/smooth-operator': patch
---

SECURITY (Python server): scope sessions by owner only when an owner exists (Option B, th-909995)

The th-8fe998 scoping rule fail-closed on any principal without an email claim. On an
auth-enabled server that locked out **anonymous connections and authenticated-but-emailless
principals entirely** — the session they had just created was ownerless, so `list_conversations`
returned empty, resume minted a fresh conversation, and `get_session` / `get_conversation_messages`
/ `send_message` all answered `SESSION_NOT_FOUND`. The identical rule in the .NET twin hung CI on
a WebSocket ACL test and was reverted in #309.

Option B: a session that HAS an owner is still owner-checked (case/whitespace-insensitive email
match); a session with NO owner — anonymous, emailless, or predating ownership — is reachable, as
it was before scoping shipped. An emailless scope matches no non-empty owner, so the reported P0
stays closed: authenticated A cannot read, resume, or `send_message` into authenticated B's owned
session, and a refusal appends nothing to B's log. Not-yours remains byte-identical to
never-existed (no existence oracle), and the auth-disabled single-tenant flavor is unchanged.

Option A (`email ?? sub`) was rejected: Go's anonymous principal uses the literal sub `"anonymous"`
for every visitor, so keying on `sub` would pool all anonymous visitors together and leak their
chats to each other.

`SessionStore.create_session` / `list_conversations` gain a keyword-only `enforced: bool = False`
that distinguishes "auth disabled, unscoped" from "authenticated but emailless" — both of which
present as a `None` owner.
