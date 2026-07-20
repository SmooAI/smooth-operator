---
'@smooai/smooth-operator': patch
---

SECURITY (Rust server): owner-check every sessionId-taking WebSocket action.

The per-user scoping added for the read paths left the write paths loading a
session by raw client-supplied id. `send_message` was the worst case: an
authenticated user who knew or guessed another user's `sessionId` could send a
message into that session — the turn replayed the victim's conversation history
as context and streamed the agent's reply back to the *sender*, so the write
hole was also a read of the victim's conversation. `get_session`, `verify_otp`,
`confirm_tool_action`, `submit_interaction` and `rename_conversation` had the
same gap.

All of them now route through a single `scoped_session` chokepoint (mirroring
the Go dispatcher's `scopedSession`): it loads the session and hides it unless
the connection's authenticated principal owns its conversation, returning
exactly what an unknown id returns — so "not yours" is byte-identical to "never
existed" and cannot be used as an existence oracle. A storage error is a denial.
Unauthenticated single-user deployments (the `th` daemon / `LocalServer`
embedding) stay unscoped, and org scoping remains as defense in depth.
