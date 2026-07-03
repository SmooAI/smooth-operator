# @smooai/smooth-extension-sdk

## 0.4.0

### Minor Changes

- a36ee69: SEP Phase 3 (SDK + spec) — the `ui/request` surface.

  The extension SDK now exposes the capability-negotiated UI surface. An extension
  reads the host's declared `ui_capabilities` from the `initialize` handshake and
  gates on `smooth.hasUI(kind)` / `ctx.hasUI(kind)`; `ctx.ui` (and `smooth.ui`)
  speak `ui/request` back to the host: `select`/`confirm`/`input` return the user's
  answer (or `{ cancelled: true }`), and `notify`/`setStatus`/`setWidget`/`setTitle`
  push to the frontend. A headless or uncapable host rejects with `RpcError` code
  -32001 (NoUI). `createTestHost(ext, { onUiRequest })` scripts the host side; its
  default mimics a headless frontend.

  Ships the `todo` demo extension (pi's todo, ported): stateful list whose tools
  push a `keyvalue` `set_widget` render block and whose `clear` asks for `confirm`
  first — both `hasUI`-gated, so it degrades cleanly headless.

  Extends `spec/extension/conformance/fixtures.json` with the remaining `ui/request`
  kinds (input/notify/set_status/set_widget/set_title), select/input/cancelled
  results, and invalid cases (unknown kind, missing `options`/`message`, extra
  property).

## 0.3.0

### Minor Changes

- 1c8f26f: SEP Phase 2 (SDK + spec) — hooks + the observe event bus.

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

## 0.2.0

### Minor Changes

- 940560b: Add the SEP TypeScript extension SDK — Phase 1 (the tool path).

  New published package `@smooai/smooth-extension-sdk`: build Smooth Extension Protocol
  extensions in TypeScript. `defineExtension`/`defineTool` (zod v4 via `z.toJSONSchema`, with
  raw JSON-Schema / TypeBox pass-through), a symmetric JSON-RPC 2.0 `Peer`, an ndjson stdio
  transport (plus an in-memory `linkedPair`), `createTestHost` for driving an extension
  in-process, and `runConformance` to replay the shared fixtures against a real extension
  subprocess. Ships the `hello` demo extension (`hello.greet` — zod schema, streamed
  `tool/update` progress, `$/cancel` cancellation). Wired into the TypeScript CI lane.

  Extends `spec/extension/conformance/fixtures.json` for the tool path: `is_error` and
  `details` tool results, a message-only `tool/update`, and invalid fixtures (missing
  `content`, out-of-range `progress`).
