---
'@smooai/smooth-operator': minor
---

**SECURITY (.NET server) — cross-user conversation data leak.** `list_conversations` returned EVERY
user's conversations, and `create_conversation_session` resume, `get_conversation_messages`, and
`get_session` performed no ownership check. Any authenticated user could enumerate and open anyone
else's chats. The conversation surface is now scoped to the connection's **authenticated principal**.

What changed:

- `Principal` carries `Email` (init-only, lifted from the validated token's `email` claim), and
  `AccessContext` carries `AuthEnabled` — which distinguishes "no auth configured" from "auth on but
  this token is anonymous". The second case now fails closed instead of inheriting unscoped behavior.
- `create_conversation_session` stamps the **principal's** email as the session owner. The frame's
  client-supplied `userEmail` is honoured only when no auth is configured at all — supplying someone
  else's email no longer buys you their scope.
- Resume, `get_conversation_messages`, and `get_session` are owner-checked. A conversation/session you
  do not own returns `SESSION_NOT_FOUND` with a payload **byte-identical** to one that never existed,
  so the error cannot be used as an existence oracle to enumerate other users' ids. (This includes
  resume of an unknown id, which previously minted a fresh conversation — under auth it now returns
  the same `SESSION_NOT_FOUND`.)
- Conversations with no recorded owner (rows written before scoping existed) belong to nobody and are
  invisible to every authenticated user.
- Auth-disabled single-tenant local/dev servers are **unchanged**: unscoped, no ownership checks.

**BREAKING for anyone implementing `ISessionStore`** (this compile break is deliberate — an optional
parameter defaulting to "no filter" would be fail-open and would leave downstream stores silently
vulnerable):

- `ListConversationsAsync(CancellationToken)` → `ListConversationsAsync(ConversationScope scope, CancellationToken)`.
  Apply the scope **inside your query** (`WHERE user_email = …`), never as a post-hoc filter in
  C# — the dispatcher applies its `LIMIT` to what you return, so filtering afterwards yields short or
  empty pages. `ConversationScope.Unscoped` returns every user's conversations and is legitimate ONLY
  on a server with no auth configured; `ConversationScope.None` returns nothing.
- New required member `ConversationBelongsToUserAsync(string conversationId, string userEmail, CancellationToken)`.
  It MUST return `false` — indistinguishably — for a conversation that does not exist, one owned by
  another user, and one with no recorded owner.

Hosts that pass identity to the server must ensure the token carries an `email` claim; a principal
without one now sees no conversations rather than everyone's.
