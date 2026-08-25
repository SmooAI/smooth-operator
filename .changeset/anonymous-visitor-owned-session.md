---
"@smooai/smooth-operator": patch
---

fix(server): an anonymous widget visitor who gives an email is no longer locked out of its own session

Seen in production on smoo.ai, as a total outage of the public chat.
`create_conversation_session { agentId, userEmail }` answered 200, and the very
next `send_message` on the same socket answered `SESSION_NOT_FOUND` for a
session that plainly existed. `userEmail` alone was the trigger — the same
create without it, or with only `userName` or `browserFingerprint`, streamed
fine.

The widget's pre-chat form collects name + email, so the email lands on the
visitor's own `user` participant, and `may_read_conversation` counts any `user`
participant with a non-blank email as making the conversation **owned**. A
public widget visitor has no verified principal, which on a multi-user
deployment is `UserScope::Denied`, whose arm was `!owned`. So the visitor was
owner-checked against an identity it does not have, and denied the session it
had itself created one frame earlier. The widget's recovery path then created a
fresh session carrying the same email and was denied identically — so real
visitors saw an unbounded retry loop and "We couldn't reach the chat", not a
transient blip. This is th-909995 recurring for the emailful case, against
`server::anonymous_scope`'s own assertion that "it can still create a fresh
session, so the anonymous widget flow keeps working": it could create, but not
use.

An anonymous connection can never satisfy an ownership check, so it no longer
faces one — narrowly:

- The exception applies only to a read reached **by id** (new `Reach::ById`),
  where the unguessable session/conversation id is the visitor's entire
  capability, exactly as it was before scoping shipped.
- `list_conversations` (`Reach::Listing`) stays strict for everyone. Anonymous
  listing falls back to the SEED org, which is precisely where widget
  conversations pool, so granting the exception there would have leaked
  visitors' chats to each other. A negative control caught that before it
  shipped.
- Keyed on "no verified principal" (`auth_org.is_none()`, set only by the
  tokenless and degraded-token branches of `resolve_ws_access`), not on the
  scope — an authenticated principal whose token carries no `email` claim still
  fails closed.
- The tenant boundary is untouched, and the fused `SESSION_NOT_FOUND` from the
  storage-blip work still leaks nothing: "not found" and "not yours" remain
  byte-identical.

Fixed at the single `may_read_conversation` chokepoint, so `send_message`,
`get_session`, `get_conversation_messages`, `confirm_tool_action`,
`submit_interaction`, `verify_otp` and conversation resume all change together.

`rust/smooth-operator-server/tests/user_scoping.rs` gains the create-**with**-
`userEmail`-then-send round trip, which is what every existing test missed: they
exercised capture and ownership separately and never the round trip a real
visitor makes. It asserts the session row exists after the create (the failure
was always an authorization denial, never a failed create), then that the send
reaches the turn. Four negative controls ride with it: `SESSION_NOT_FOUND` is
still producible for that same caller on an unknown id, an authenticated
emailless principal still cannot reach an owned session, another authenticated
user still cannot reach the visitor's session or see it in a list, and an
anonymous connection still cannot enumerate authenticated users' conversations.

No API change.
