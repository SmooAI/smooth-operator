---
'@smooai/smooth-operator': patch
---

SECURITY (.NET server): scope the conversation WRITE path, not just the reads.

th-966fab owner-checked `get_session` / `get_conversation_messages` / resume, but
`send_message` still loaded any session by client-supplied `sessionId`. An
authenticated user who knew (or guessed) another user's `sessionId` could send a
message into that session — the turn replayed the victim's conversation history as
context and streamed the agent's reply back to the *attacker*. A read of someone
else's conversation dressed up as a write, defeating the read scoping entirely.
`verify_otp` and `confirm_tool_action` were unscoped the same way (marking a
foreign session identity-verified; approving a foreign parked write).

The fix adopts the Go server's chokepoint pattern: a single private
`ScopedSessionAsync` is now the only way a handler may turn a client-supplied
`sessionId` into a session. It hides a session the connection's principal doesn't
own by returning exactly what an unknown id returns, so every caller emits the
identical not-found response and "not yours" stays indistinguishable from "never
existed". All five sessionId-taking handlers route through it.

The visibility rule is "Option B": a session that HAS an owner is owner-checked; a
session with NO owner is reachable. A first attempt (#308) also denied ownerless
sessions and emailless principals outright, and was reverted (#309) — an
authenticated principal whose token carries no `email` claim, and an anonymous
connection to an auth-enabled server, both stamp `ownerEmail = null` at
`create_conversation_session` and were then refused by their own session on the next
`send_message`. That is not "denied someone else's history", it is "cannot use the
product": it killed anonymous/public-agent chat, and hung the .NET integration suite,
whose ACL test converses over exactly that path. Ownerless sessions remain absent from
`list_conversations` and non-resumable by `conversationId`, so reaching one requires
already holding its `sessionId`. No behavior change when auth is disabled
(single-tenant local/dev stays unscoped).
