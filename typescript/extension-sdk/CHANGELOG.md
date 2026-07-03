# @smooai/smooth-extension-sdk

## 0.6.0

### Minor Changes

- 21016e5: SEP Phase 8 (spec + SDK + demo) — long-tail pi parity.

  **Spec.** `initialize.schema.json` registrations gain `hooks` (declared intercept
  hooks, so the host can skip the per-turn `context` hook) and `message_renderers`
  (declarative `tag` → render-block templates). New `RenderBlock` `$def` — the
  render-block DSL (`markdown`/`keyvalue`/`table`/`diff`/`progress`/`stack` + the
  interactive `widget` kind with keybindings, each with a `text` fallback) — plus
  `MessageRendererRegistration`. `ui/request` `set_widget` documents its widget as a
  render block (kept permissive since SEP carries no cross-file `$ref`s). New
  conformance fixtures: `event_bus_fanout` (`bus/event`), `event_widget_key`
  (`widget/key`), `registrations_phase8` (hooks + message renderer), and
  `render_block_widget`.

  **SDK.** `render.*` builders for the render-block DSL; `smooth.events`
  (`publish`/`on`) for the inter-extension bus; `smooth.registerMessageRenderer(tag,
template)`; `ctx.ui.setWidget` now takes a typed `RenderBlock`; the `context` +
  `before_agent_start` hooks and `widget/key` events are exercised end-to-end.
  `buildRegistrations` emits `hooks` + `message_renderers`. `createTestHost` records
  `bus/publish` (`busPublishes`) and services it. New `eventName` constants
  (`BUS_EVENT`, `WIDGET_KEY`) and `method.BUS_PUBLISH`.

  **Demo.** `snake` — pi's game ported to the render-block v2 widget DSL: `play`
  pushes an interactive `widget` block; each `widget/key` advances a pure game core
  and re-renders. Full-fidelity on web, reduced-fidelity (ASCII grid + score) on the
  TUI, identical keybinding DSL.

  **Docs + scaffold.** `PORTING.md` — the pi → SEP parity checklist (every pi
  `ExtensionAPI` member → equivalent, port delta, or documented N/A). New `provider`
  scaffold template in `create-smooth-extension` (registers a provider; builds and
  tests green with a canned response, marked where the real call goes).

## 0.5.0

### Minor Changes

- 0953584: SEP Phase 4 (spec + SDK) — commands, flags, shortcuts, and session actions.

  **Spec.** New `command-complete.schema.json` (argument autocomplete). `session.schema.json` now carries the dispatch `context` on every params object (the wire form of the command-tier + epoch guard the host enforces) and adds `send_user_message` (`deliver_as` steer/follow_up/next_turn). `initialize.schema.json` gains a `flags` delivery map on the params and a `shortcuts` list (+ `ShortcutRegistration`) on the registrations. New conformance fixtures for command/complete, session send_user_message/append_entry, shortcuts, and flag delivery; new `$invalid` cases proving `context` is required on a session action and `value` on a completion. The reference `echo.mjs` registers a command + shortcut and answers command/execute + command/complete.

  **SDK.** `smooth.registerCommand` (with an optional `complete` completer), `registerFlag` (+ `smooth.getFlag`), and `registerShortcut`. Command handlers receive a `CommandContext` bound to their command-tier context, exposing `session.sendMessage` / `sendUserMessage` / `appendEntry`, `ui`, `hasUI`, and `args`. `createTestHost` gains `runCommand`, `completeCommand`, and a `session/*` service that enforces the same command-tier guard the engine does (event-tier → -32003), recording every session call for assertions. `runConformance` now replays command/execute + command/complete.

  **Demo.** `plan-mode` — the flagship extension that exercises phases 2–4 together: a `--plan` flag and a `/plan` command toggle plan mode; a `tool_call` intercept blocks write/edit/apply_patch/bash while it is on; each toggle pushes a `set_widget` render block and persists an LLM-invisible `appendEntry`, so the state survives a hot reload (the flag re-seeds it, the transcript keeps the history).

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
