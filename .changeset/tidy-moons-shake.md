---
'@smooai/smooth-operator': minor
---

**SECURITY — Python server: per-user conversation scoping (th-8fe998).** Fixes a cross-user data leak: `list_conversations` took no user filter and returned **every** user's conversations, and neither the resume path nor the sessionId-bearing actions were owner-checked, so any authenticated user could enumerate and open anyone else's chats.

Conversations are now owned by the **authenticated principal's** email (the JWT `email` claim, plumbed onto `Principal`) — never the client-supplied `userName` / `userEmail` frame fields, which were the spoofing vector. With auth enabled the principal's email also replaces `userEmail` as the OTP contact, so a verification code can't be delivered to a client-chosen address.

- `list_conversations` is scoped to the caller, with the filter applied **in the store's selection** — not after the dispatcher's limit, which would silently return short or empty pages.
- `create_conversation_session` (resume), `get_session`, `send_message`, and `verify_otp` are owner-checked. Someone else's id is reported **byte-identically** to an id that never existed — the resume path mints a fresh conversation, the rest return the same `SESSION_NOT_FOUND` payload — so none of them can be used as an existence oracle to enumerate other users' ids.
- Fail-closed: auth enabled + a principal with no email lists nothing and can resume nothing; it never falls back to unscoped. A session stored with no owner is invisible to everyone. Auth **disabled** (the local single-tenant flavor) is the only unscoped path and is unchanged.

**BREAKING (`SessionStore` implementers).** `list_conversations` now takes a **required** `user_email` parameter — deliberately not an optional defaulting to `None`/unscoped, which would be fail-open and would let a downstream store ship cross-user-leaking without ever confronting the question. `create_session` gains a keyword-only `owner_email`, and `StoredSession` gains `owner_email`.

Migration: pass the authenticated principal's email through both and filter your selection by it. Pass `None` **only** for a single-tenant, auth-disabled deployment, where it means "unscoped". If you implement this protocol in your own store, treat a not-owned row exactly as a missing one.
