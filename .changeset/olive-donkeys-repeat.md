---
'@smooai/smooth-operator': minor
---

Fix silently-dropped protocol frames in the Go, Python, .NET and TypeScript clients, and add the `submit_interaction` verb to Go/Python/.NET.

The generated wire types for `stream_preamble`, `stream_reasoning`, `interaction_required` and `interaction_invalid` existed in every language, but the hand-maintained dispatch unions were never updated. Every one of those frames was rejected by each client's own frame guard and then discarded by its dispatch loop — Go's `ParseServerEvent` returned `UnknownEventError` and the read loop `continue`d, Python's `parse_event` raised and `_handle_frame` swallowed it, and .NET's `ServerEventConverter` threw a `JsonException` the client caught and dropped.

Impact: a session declaring a Rich Interaction (`identity_form`, `choice_chips`) parked a turn that Go/Python/.NET consumers never saw — the turn hung to the turn timeout (and forever on .NET, which has none). `stream_preamble` and `stream_reasoning` are emitted by the production server today, so preamble and reasoning tokens were being discarded by all four clients, TypeScript included.

- **Go** — added the four event discriminators plus `As*` accessors, `ActionSubmitInteraction`, and `Client.SubmitInteraction`.
- **Python** — added the four members to `EventType` and the `ServerEvent` union, `submit_interaction` to `ActionType` and `ClientAction`, `SmoothAgentClient.submit_interaction`, and the missing entries in `validate.py`'s schema maps.
- **.NET** — added `StreamPreambleEvent`, `StreamReasoningEvent`, `InteractionRequiredEvent`, `InteractionInvalidEvent`, wired them into `ServerEventConverter` and `EventTypes.All`, and added `SubmitInteractionAction` + `SmoothAgentClient.SubmitInteractionAsync`.
- **TypeScript** — added the missing `stream_reasoning` to `EVENT_TYPES`/`ServerEvent` and to `validate.ts`'s event→schema map. (The shipped web-chat example already had a `case 'stream_reasoning'` that could never fire.)

Each language also gains a drift guard that derives the expected discriminator set from `spec/events/*.schema.json` and `spec/actions/*.schema.json` at test time, so a future event schema that isn't wired into a dispatch union fails the build instead of being dropped at runtime.
