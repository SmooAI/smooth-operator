---
'@smooai/smooth-operator': patch
---

SECURITY: fix cross-user conversation-history leak in the Python server's `get_conversation_messages`.

The per-user scoping fix added a `_visible_session` ownership chokepoint and routed
`get_session`, `send_message` and `verify_otp` through it, but `get_conversation_messages`
still called the store directly — so any authenticated user could read any other user's
full conversation history by sessionId. It now routes through the same chokepoint and
reports a session it does not own with the byte-identical `SESSION_NOT_FOUND` payload it
uses for an id that never existed (no existence oracle). A structural test now fails if any
future handler bypasses the chokepoint again.
