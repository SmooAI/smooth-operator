---
'@smooai/smooth-operator-js': minor
---

SMOODEV-3412: `AppState::with_require_owned_conversations(true)` — let an org-authenticated host make ownerless conversations unreachable.

`may_read_conversation` treats a conversation with no `user` participant as open to every principal. That is deliberate and load-bearing for anonymous/widget flows (th-909995): those principals own nothing, and the conversations they create are exactly the ownerless ones, so denying them locked callers out of their own sessions.

An **org-authenticated** pot is the opposite case. Every caller is a real user, and the ownerless conversations in its org are the ones machines made — phone calls, SMS, widget chats. In SmooAI's `copilot-ws` that meant the operator's history picker listed **customer conversations**, and could resume them: a stored pointer bound a new session to a customer's phone call on every drawer open.

The new flag makes ownerless unlistable, unresumable and unreadable. **Off by default**, so no existing host changes behaviour.

It is checked before the scope match, so it covers `UserScope::Denied` too: an emailless principal owns nothing and must reach nothing. The flag therefore **fails closed** — enabling it with a verifier whose principals carry no `email` claim breaks the picker rather than leaking through it. Ship the claim first.
