---
'@smooai/smooth-operator': minor
---

**SECURITY** — fix a cross-user conversation data leak in the TypeScript server, and scope every conversation read to the connection's authenticated principal (th-8fe998).

**The vulnerability.** `SessionStore.listConversations()` took no user filter, so the `list_conversations` action returned EVERY user's conversations to any caller. The resume path (`create_conversation_session` with a `conversationId`) and `get_conversation_messages` performed no owner check either, so any authenticated user could enumerate other users' conversation ids from the list and then open, read, and post into those conversations. `get_session`, `send_message`, and `verify_otp` were exposed through the same missing check.

**The fix.** A session now records an owner — the authenticated principal's email, taken from the connection's `email` claim. Every conversation read is checked against it:

- `list_conversations` is scoped to the principal, with the filter applied inside the store selection (ahead of any limit, so a scoped page is never silently short or empty).
- `get_session`, `get_conversation_messages`, `send_message`, and `verify_otp` return `SESSION_NOT_FOUND` for a session the caller doesn't own — byte-identical to the response for an id that never existed, so the pair can't be used as an existence oracle to enumerate other users' session ids.
- Resuming another user's conversation is treated exactly like resuming an unknown id: the id is dropped and a fresh conversation is minted. Erroring on a real-but-not-yours id while silently minting for an unknown one would itself confirm which ids exist.
- The client-supplied `userName` / `userEmail` frame fields no longer determine identity. They were the spoofing vector: a caller could claim any email and receive that user's scope. The principal always wins; on an auth-enabled server the frame values are ignored for ownership (`userEmail` still serves as the OTP delivery contact).

**Fail-closed rules.** Auth enabled and the principal has an email → scoped to it. Auth enabled and the principal is missing or emailless (including a missing, expired, or forged token) → empty list and denied reads, never a silent fall back to unscoped. Auth disabled (no verifier configured — local/dev single-tenant) → unscoped, unchanged; this is the only unscoped path.

**BREAKING for custom `SessionStore` implementations** — deliberately, and the break is the point:

- `listConversations()` gains a **required** `userEmail: string | undefined` parameter. It is required, not optional-defaulting-to-unscoped, because an optional parameter is fail-OPEN: existing implementations would keep compiling and keep leaking every user's conversations. The compile error forces each implementation to make an explicit scoping decision.
- `getConversation()` now returns `{ conversationId, userEmail }`, with `userEmail` required so a store that doesn't track ownership fails to compile rather than silently reporting every conversation as ownerless.
- `StoredSession` gains an optional `userEmail` (the owner). Implementations must persist it at create time and must NOT let a resume rewrite it, or a second caller could take ownership of a conversation by resuming it.

Migration: filter conversations by the passed `userEmail` in the query itself (`WHERE user_email = ?`), never after applying a limit; return `undefined` for `userEmail` only when the row genuinely has no owner. `AccessContext` also gains a required `authEnabled` flag, set by the verifier, which distinguishes "auth is off" (unscoped) from "auth is on but this connection didn't authenticate" (fail closed) — custom `AuthVerifier` implementations must set it.
