---
"@smooai/smooth-operator": patch
---

feat(server): a host can install the turn's `AgentExecutor` — the missing seam on the emitted reply

`TurnRequest::executor` has been a public field since ADR-030, and
`runner::turn_executor` has always honored it, but the server's **sole**
`TurnRequest` construction site (`handler.rs`) hardcoded `executor: None`. So the
seam existed on paper and was unreachable in practice: nothing outside this crate
could supply one.

That gap is what left chat-ws with no host-side seam on the emitted text. When the
runner owns the whole turn and streams plain text from inside the published crate,
a host has no point at which to inspect what the agent said next to what it
actually did — so the TS general agent's post-response guard (which STRIPPED an
escalation claim when `notify_humans` had not fired) and the voice stall-reply
retry had nowhere to run. The consequence was live: an agent could tell a customer
"I've passed it along" with nothing behind it, and the only available fix was
prompt prevention, not enforcement.

`AppState::with_executor` installs one, and the handler passes it onto every turn.
Two things arrive through it:

- a **durable backend** (ADR-030) — the case the trait was written for; and
- a **decorator**: an executor that delegates to `InProcessExecutor` and then
  inspects or edits the returned `Conversation` before the runner reads its final
  assistant message. `Conversation.messages` is public and carries the turn's tool
  calls, so this is the one place a host can guard a reply against the tools that
  actually ran.

One boundary is worth stating plainly rather than discovering later: tokens the
turn streamed have already left over the events channel by the time the
conversation is returned, so an edit here changes the persisted message and the
`eventual_response` — not what already streamed. A decorator that needs the stream
too can pass its own channel down and forward.

Default behavior is unchanged: `None` is still the in-process executor, which is a
verbatim delegation to `Agent::run_with_channel`. The lambda flavor keeps `None`
alongside its other `None` injection seams (it has no `AppState` to install one
on).

`rust/smooth-operator-server/tests/executor_seam.rs` drives the real
`handle_frame` offline with an escalation-guard executor and pins both halves: the
installed executor is the one that runs the turn and its rewrite reaches BOTH the
persisted outbound message and the `eventual_response`; with no executor installed
the model's text survives byte-for-byte.
