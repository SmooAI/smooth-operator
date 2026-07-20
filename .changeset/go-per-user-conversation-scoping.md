---
'@smooai/smooth-operator': minor
---

**SECURITY (cross-user data leak) — Go server: scope conversations to the authenticated user.**

`SessionStore.ListConversations` took no user filter, so `list_conversations` returned **every user's conversations** to any authenticated caller. The resume path and `get_conversation_messages` were not owner-checked either, so a caller could also open and read another user's conversation by id. Any authenticated user could enumerate and read anyone else's chats.

Conversations are now owned by the **authenticated principal** and every read is filtered by it:

- `Principal` carries `Email` (the JWT `email` claim); `AccessContext.ConversationScope()` derives the connection's visibility. Ownership comes from the connection's principal only — the client-supplied `userName` / `userEmail` frame fields were the spoofing vector and no longer influence who may read what (`userEmail` still serves as the OTP delivery contact).
- `ListConversations` filters during selection, before the handler's limit — filtering after a limit silently returns short/empty pages.
- `get_session`, `get_conversation_messages`, `send_message` and `verify_otp` all route session lookups through one owner-checked chokepoint.
- **Not-yours is indistinguishable from never-existed.** A denied session read returns the identical `SESSION_NOT_FOUND` payload an unknown id returns, and resuming another user's conversation mints a fresh conversation exactly as an unknown id does — so neither path can be used as an oracle to enumerate other users' session or conversation ids.
- Fails **closed**: auth enabled with a principal that has no email (including a rejected/expired token) sees nothing, rather than falling back to unscoped.
- Auth **disabled** (no verifier configured — local/dev single-tenant) stays unscoped and is unchanged. That is the only unscoped path.

**BREAKING for `SessionStore` implementers (deliberate).** `ListConversations`, `CreateSession` and `ResumeSession` now take a required `ConversationScope`. The parameter is required rather than optional-defaulting-to-unscoped precisely so that every downstream implementation gets a **compile error** and must confront who may see what; a default-to-unscoped parameter would be fail-open and would leave downstream stores silently vulnerable.

Migration: thread the scope from `AccessContext.ConversationScope()` into your store, persist the owning email on conversation creation, and filter reads by `scope.Allows(ownerEmail)`. `ConversationScope`'s zero value denies everything, so a partially-migrated store leaks nothing.
