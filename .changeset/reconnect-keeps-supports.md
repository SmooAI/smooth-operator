---
"@smooai/smooth-operator": patch
---

fix(all five): a reconnect no longer silently turns Rich Interactions off

`supports` — the client render-capability list that gates the **entire** Rich
Interactions framework — was kept somewhere that a reconnect destroys, in every
implementation.

A **reconnect is a resume**: the client re-opens the socket and re-issues
`create_conversation_session` with the same `conversationId`, which mints a
**new session id** on a **new dispatcher**. So unless the client re-declared
`supports` on every single reconnect, the server forgot the client could render
cards at all and every interaction kind quietly fell back to conversational
collection — no error, no event, nothing on the wire to notice. The parked-card
flow (raise tool → `interaction_required` → `submit_interaction` → resume)
simply stopped happening. Reconnects are routine (network blips, mobile
backgrounding, deploys), so a shipped feature was degrading in the field with no
signal.

- **Rust** kept it in `Session.metadata.supports` — the per-pod session registry.
- **Go** (`FrameDispatcher.supports`), **Python** (`_session_supports`) and
  **.NET** (`_sessionSupports`) kept a per-connection map, also never pruned.
- **TypeScript** already stored it through the `SessionStore`, but on the
  **session** record, so a resumed session started empty just the same.

The session was already the wrong home, and the repo had said so once: th-c12df5
moved the workflow step pointer off it for exactly this reason ("this per-pod
session map resets on reconnect/pod hop"). `supports` now lives on the
**conversation** in all five, mirroring whatever conversation-scoped mechanism
each store already had — Rust/Go/Python/TypeScript write `clientSupports` into
`conversations.metadata_json`; .NET follows its own store's documented hold for
`currentStepId`/`otpVerified` (session-row metadata) under the same key name.

A list the frame **does** declare always wins, including `[]` — which is now how
a text-only channel resuming a rich conversation opts out, and the opt-out is
durable so the next reconnect that omits the key cannot resurrect the old
capabilities. Because `[]` and an absent key now mean different things, each port
had to stop collapsing them (`*[]string` in Go, `IReadOnlyList<string>?` in .NET,
`undefined` vs `[]` across the TS store interface).

The rule lives in the `supports` description in
`spec/actions/create-conversation-session.schema.json` — the source of truth —
and the TS/Go/Python/.NET wire types are regenerated from it rather than
restating it by hand. Each implementation adds a reconnect test that drives a
**second, fresh dispatcher over the same store**, and each was verified to fail
against its own pre-fix code.
