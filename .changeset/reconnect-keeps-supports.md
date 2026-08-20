---
"@smooai/smooth-operator": patch
---

fix(rust,spec): a reconnect no longer silently turns Rich Interactions off

`supports` — the client render-capability list that gates the whole Rich
Interactions framework — lived only on the session (`Session.metadata.supports`,
read by `AppState::session_capabilities`). A **reconnect is a resume**: the
client re-opens the socket and re-issues `create_conversation_session` with the
same `conversationId`, which mints a **new session id**. So unless the client
re-declared `supports` on every reconnect, the server forgot the client could
render cards at all, and every interaction kind quietly fell back to
conversational collection — no error, no event, nothing on the wire to notice.
The parked-card flow (raise tool → `interaction_required` → `submit_interaction`
→ resume) simply stopped happening. Reconnects are routine (network blips,
mobile backgrounding, deploys), so this was a shipped feature degrading in the
field with no signal.

The session registry was already the wrong home for this, and the repo had
already said so once: th-c12df5 moved the workflow step pointer off it for
exactly this reason ("this per-pod session map resets on reconnect/pod hop").
`supports` now rides durable **conversation** metadata (`clientSupports`) the
same way, so a resume that omits the key inherits what the conversation last
declared.

A list the frame *does* declare always wins — including `[]`, which is now how a
text-only channel resuming a rich conversation says "I render nothing" (the
schema's `supports` description carries the rule, and the generated TS/Go/Python/
.NET types are regenerated from it). Both directions are covered by a new
integration test, `reconnect_resuming_a_conversation_keeps_the_declared_
capabilities`, which fails on the pre-fix handler.

**Unchanged in the four ports.** Go, TypeScript, Python and .NET never read
`supports` at all and host no interactions framework, so there is nothing there
to keep across a reconnect yet; the Rust reference is where the behavior is
defined.
