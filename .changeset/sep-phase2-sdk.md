---
"@smooai/smooth-extension-sdk": minor
"@smooai/smooth-operator": patch
---

SEP Phase 2 (SDK + spec) — hooks + the observe event bus.

`@smooai/smooth-extension-sdk` gains **hook handlers**: `smooth.on(name, handler)`
now covers both observe events (return ignored) and intercept hooks (return a
`HookResult` — `{ block, reason? }` to veto or `{ patch }` to rewrite the input).
The extension answers the `hook` request by folding its own handlers in
registration order (first `block` short-circuits; `patch`es shallow-merge and
thread to the next), and the host chains the outcome across extensions. Hook
names are kept out of the reported event `subscriptions`. `createTestHost` gains
`callHook(hook, input)`; new `permission-gate` demo extension blocks dangerous
`bash` commands via a fail-closed `tool_call` hook.

`spec/extension`: the event schema gains an optional `seq` (per-connection
monotonic sequence; absent on the out-of-band `events_lost` marker) with a
`model_select → AgentEvent::ModelResolved` parity note, and fixtures add a
seq-numbered event, the `events_lost` marker (drop-N → count), a
`tool_execution_start` event, and the `tool_result` hook input + a result-shaped
`modify` outcome. Rust and TypeScript conformance replays stay green.
