# @smooai/smooth-operator

## 1.20.0

### Minor Changes

- af9ac05: Suggested quick replies: the Rust server's `eventual_response` now carries live `suggestedNextActions` instead of a hardcoded empty array. The runner appends a machine-parsed trailer contract (`<suggested_replies>["…"]</suggested_replies>`) to every turn's system prompt, suppresses the trailer from the live token stream, strips it from the persisted/final reply, and surfaces the parsed suggestions (capped at 4) on `TurnResult.suggested_next_actions` and the `eventual_response` payload. `runner::general_agent_response` now takes the suggestions slice. Rust server only; other language servers still emit an empty array (parity follow-up).

## 1.19.0

### Minor Changes

- 3a9d29e: Identity intake — a channel-normalized lead/identity capture primitive (`docs/Architecture/Identity Intake.md`). New protocol surface: `supports` client-capability declaration on `create_conversation_session`, `identity_intake_required` / `identity_intake_invalid` events, and the `submit_identity_intake` resume action (with server-side validation: required fields, email shape, E.164 phone normalization). Rust reference implementation: `request_identity_intake` / `submit_identity_intake` agent tools in `smooai-smooth-operator` (park-and-resume on form-capable sessions; validated conversational turn-by-turn fallback on text-only channels — both resume with the same structured payload), server wiring (pending-intake registry, session identity attach onto the OTP contact keys) in `smooai-smooth-operator-server`. TypeScript client: regenerated spec types, `supports` on `createConversationSession`, and the `submitIdentityIntake()` resume verb. Parity for the TS/Python/Go/.NET servers is tracked as follow-ups; the spec + conformance fixtures are the complete contract.

## 1.18.0

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

## 1.17.0

### Minor Changes

- f370ae9: SEP — the .NET operator server (`dotnet/server`) now hosts extensions (ui/confirm producer).

  The C# server wires the engine `ExtensionHost` (from `SmooAI.SmoothOperator.Core` 1.4.0)
  into each `send_message` turn. With `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny allowlist —
  the server has no interactive trust prompt), `ExtensionServerHost.BuildAsync` discovers
  `extension.toml` extensions, spawns them as JSON-RPC/ndjson subprocesses, and exposes their
  tools. Those tools join the turn's tool set so they flow through the SAME per-agent
  `enabled_tools` filtering + auth gate as native tools (dotted `<ext>.<tool>` names match
  `toolId`), and the host is torn down (subprocesses killed) at turn end.

  An extension's `ui/confirm` bridges onto the operator protocol's
  `write_confirmation_required`/`confirm_tool_action` frames via `ConfirmUiProvider` — parking
  on the same session-keyed `ConfirmationRegistry` the native write-tool HITL uses. Every other
  `ui/*` degrades headless. Only the `confirm` capability is advertised at handshake.

  Additive: with the allowlist empty (the default) no host is ever built, so behavior is
  byte-for-byte unchanged. Verified by an integration test that runs the spec's Node echo peer
  through a real server turn and asserts `enabled_tools` filtering drops an extension tool
  exactly like a native one.

## 1.16.0

### Minor Changes

- 49bd798: SEP — the TypeScript operator server now hosts extensions (`ui/*` producer),
  mirroring the Rust reference (`rust/smooth-operator-server/src/extensions.rs`).

  `typescript/server` wires the engine `ExtensionHost`
  (`@smooai/smooth-operator-core/extension`) into each turn: with
  `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny, comma-separated trust allow-list)
  it discovers `extension.toml` extensions, spawns them as JSON-RPC/ndjson
  subprocesses, and registers their `<ext>.<tool>` tools into the turn's tool set
  BEFORE the per-agent `enabled_tools` filter — so an allow-list drops them exactly
  like a built-in (SMOODEV-590 parity). A `ConfirmUiProvider` bridges an
  extension's `ui/confirm` onto the existing `write_confirmation_required` /
  `confirm_tool_action` frames via the session-keyed `ConfirmationRegistry`; every
  other `ui/*` degrades headless (render-only → `{}`, select/input → `{cancelled}`).
  The host and its subprocesses are torn down at turn end. Unset
  `SMOOTH_EXTENSIONS_ALLOW` (the default) builds no host — behavior is unchanged.

## 1.15.1

### Patch Changes

- 35806b2: Go server: host SEP extensions in a turn + ui/confirm bridge (th-829d9f).

  Wires the engine's SEP `ExtensionHost` (new in smooth-operator-core) into the Go
  operator server's send_message turn:

  - **Default-deny discovery** — `SMOOTH_EXTENSIONS_ALLOW` (comma-separated names)
    is the trust decision; empty (the default) builds no host, so behavior is
    byte-for-byte unchanged. Allowlisted `extension.toml` extensions are discovered
    (`SMOOTH_EXTENSIONS_DIR` or the engine default) and spawned per turn.
  - **Tool composition** — an extension's tools (`<ext>.<tool>`) are folded into the
    turn's tool set before the SMOODEV-590 `enabled_tools` / authLevel filter, so
    they gate exactly like a built-in tool.
  - **ui/confirm bridge** — `confirmUIProvider` projects an extension's `ui/confirm`
    onto the existing `write_confirmation_required` / `confirm_tool_action` frames via
    the per-connection confirmation registry; other `ui/*` degrade headless.

  Covered by an end-to-end test that drives a scripted model calling an
  extension-registered tool through the real WS/dispatcher turn (echo peer via a
  self-re-exec of the test binary), asserting execution and `enabled_tools` filtering
  parity, plus default-deny. Race-clean.

## 1.15.0

### Minor Changes

- b88d39c: Python server: host SEP extensions in a turn (ui/\* producer) — pearl th-66251a.

  Wires the engine's `ExtensionHost` (ported to the Python core in smooth-operator-core#33) into the Python operator server, the Python sibling of the Rust reference server wiring (#159). A turn can now host `extension.toml` extensions: their tools reach the agent and their `ui/confirm` bridges onto the chat-native confirmation frame.

  - **Trust — default deny.** `SMOOTH_EXTENSIONS_ALLOW` (comma-separated names) IS the trust decision; empty/unset (the default) means no extension is ever spawned and the host is never built, so behavior is byte-for-byte unchanged. `SMOOTH_EXTENSIONS_DIR` overrides the discovery dir.
  - **Tools + `enabled_tools` parity.** An allowlisted extension's eager tools are added to the turn's tool set and flow through the SAME per-agent `enabled_tools` filter (`filter_tools`, by tool name) the built-ins get — so an allow-list drops an extension tool (`echo.say`) exactly like a built-in.
  - **`ui/confirm` → the confirmation frame.** `ConfirmUiProvider` (a `HostDelegate`) projects an extension's `ui/confirm` onto the existing `write_confirmation_required` / `confirm_tool_action` frames via the same session-keyed `ConfirmationRegistry` the native write HITL uses; every other `ui/*` degrades headless (interactive → `{cancelled}`, render-only → `{}`). Only the `confirm` capability is advertised at handshake.
  - **Teardown.** The per-turn host is shut down (subprocesses stopped, parked confirmation cleared) at turn end.

  New module `smooth_operator_server.extensions` (`build_extension_host`, `ConfirmUiProvider`, `parse_allowlist`), wired into `turn_runner.py`. Integration tests drive a real echo-peer extension through a live `send_message` turn (tool runs + result streams back) and assert `enabled_tools` filtering parity, plus the `ui/confirm` bridge unit tests.

## 1.14.0

### Minor Changes

- be6b62f: SEP — the Rust operator server now hosts extensions (`ui/*` producer).

  The reference operator server (`smooth-operator-server`) wires the engine
  `ExtensionHost` into each turn: with `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny
  allowlist — the server has no interactive trust prompt), it discovers
  `extension.toml` extensions, spawns them as JSON-RPC/ndjson subprocesses, and
  attaches the host to the agent. An extension's tools land in the turn's
  `ToolRegistry` and flow through the same per-agent `enabled_tools` filtering +
  authLevel gating as built-ins (SMOODEV-590), and its hooks/events run in the
  agent loop.

  `ui/confirm` is projected onto the existing `write_confirmation_required` /
  `confirm_tool_action` HITL frames — the same out-of-band bridge the native
  write-tool `ConfirmationHook` uses, so a hosted extension's confirm prompt pauses
  and resumes the turn end-to-end. Every other `ui/*` degrades headless (only the
  `confirm` capability is advertised at handshake). Unconfigured (empty allowlist),
  no host is built and behavior is byte-for-byte unchanged.

  This is the first operator server to host extensions. The other four polyglot
  servers (TypeScript, Python, Go, .NET) have the agent-loop + HITL landing pad
  wired but their engine cores have no SEP `ExtensionHost` yet — porting it to each
  engine is tracked as follow-up work.

## 1.13.0

### Minor Changes

- 70bd271: SEP Phase 7 (spec + SDK + demo) — registerProvider: declarative providers, OAuth,
  proxied streaming, and set_model.

  **Spec.** New `provider.schema.json` covering `provider/complete` (params +
  result), `provider/delta`, and `provider/oauth_login`/`oauth_refresh` (params +
  credentials). `initialize`/`registry-update` registrations gain `providers`
  (`ProviderRegistration` + `ProviderModel`); `session/set_model` params gain
  optional `provider` + `thinking`; `capabilities_enabled` gains `providers`. New
  conformance fixtures for every provider shape (valid + `$invalid`), replayed by
  both the TypeScript schema conformance test and the Rust host's vendored copy.

  **SDK.** `smooth.registerProvider(defineProvider({ name, models, complete,
oauthLogin?, oauthRefresh? }))` — the extension owns the request/stream, emitting
  `ctx.delta(event)` chunks while streaming. `session.setModel(model, { provider,
thinking })` completes the Phase 4 session surface. `createTestHost` gains
  `complete()` (with `onDelta`), `oauthLogin()`, `oauthRefresh()`, and routes
  `provider/delta` by `request_id` — the in-process mirror of the engine's
  `ProviderStreams`.

  **Demo.** `corporate-proxy` registers a provider that proxies an OpenAI-compatible
  endpoint: it streams the upstream SSE back as `provider/delta` chunks, maps
  tool-call responses, and mediates OAuth (login prompt over `ui/input`, token
  exchange). Exercised end-to-end in `provider-path.test.ts` against a real mock
  upstream serving scripted SSE.

## 1.12.0

### Minor Changes

- 7a05f00: SEP Phase 6 (chat-widget) — render agent confirmation prompts as chat-native
  buttons.

  The embeddable chat widget now renders a `write_confirmation_required` HITL
  event as an inline Yes/No button prompt inside the assistant bubble instead of
  silently ignoring it. Clicking a button sends the `confirm_tool_action` resume
  frame and un-pauses the turn; the chosen answer sticks in the transcript. This
  is the chat-native projection of SEP `ui/confirm` (a hosted extension's confirm
  prompt maps onto the existing `write_confirmation_required` frame).

  `ConversationController` gains `answerPrompt(requestId, value)` and an optional
  client-options constructor arg (a transport seam for tests). `ChatMessage` gains
  an optional `prompt` field (`ChatPrompt`) carrying the buttons; the multi-option
  shape also backs a future `ui/select` chat frame.

## 1.11.4

### Patch Changes

- 0953584: SEP Phase 4 (spec + SDK) — commands, flags, shortcuts, and session actions.

  **Spec.** New `command-complete.schema.json` (argument autocomplete). `session.schema.json` now carries the dispatch `context` on every params object (the wire form of the command-tier + epoch guard the host enforces) and adds `send_user_message` (`deliver_as` steer/follow_up/next_turn). `initialize.schema.json` gains a `flags` delivery map on the params and a `shortcuts` list (+ `ShortcutRegistration`) on the registrations. New conformance fixtures for command/complete, session send_user_message/append_entry, shortcuts, and flag delivery; new `$invalid` cases proving `context` is required on a session action and `value` on a completion. The reference `echo.mjs` registers a command + shortcut and answers command/execute + command/complete.

  **SDK.** `smooth.registerCommand` (with an optional `complete` completer), `registerFlag` (+ `smooth.getFlag`), and `registerShortcut`. Command handlers receive a `CommandContext` bound to their command-tier context, exposing `session.sendMessage` / `sendUserMessage` / `appendEntry`, `ui`, `hasUI`, and `args`. `createTestHost` gains `runCommand`, `completeCommand`, and a `session/*` service that enforces the same command-tier guard the engine does (event-tier → -32003), recording every session call for assertions. `runConformance` now replays command/execute + command/complete.

  **Demo.** `plan-mode` — the flagship extension that exercises phases 2–4 together: a `--plan` flag and a `/plan` command toggle plan mode; a `tool_call` intercept blocks write/edit/apply_patch/bash while it is on; each toggle pushes a `set_widget` render block and persists an LLM-invisible `appendEntry`, so the state survives a hot reload (the flag re-seeds it, the transcript keeps the history).

## 1.11.3

### Patch Changes

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

## 1.11.2

### Patch Changes

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

## 1.11.1

### Patch Changes

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

## 1.11.0

### Minor Changes

- ec80d14: Add the SEP (Smooth Extension Protocol) spec — Phase 0.

  New `spec/extension/` tree: `envelope.md` (JSON-RPC 2.0 over ndjson framing, method
  catalog, error codes, context tiers, deferred WS binding), `methods/*.schema.json` (draft
  2020-12, snake*case: initialize, shutdown, ping, event, hook, tool/execute, tool/update,
  $/cancel, command/execute, registry/update, tools/set_active, session/*, exec/run,
  ui/request, kv/\_, bus/publish, log, plus the JSON-RPC frame envelope), and
  `conformance/fixtures.json` (43 valid + 6 invalid instances) with the dependency-free
  `echo.mjs` demo extension. A new `extension-conformance.test.ts` validates every fixture
  against its schema, mirroring the existing operator-protocol conformance harness. SEP is a
  sibling of the operator WebSocket protocol — it reuses the spec machinery, not the
  envelope.

## 1.10.4

### Patch Changes

- 00b2623: C# server: OTP / session-identity seam parity with the Rust reference (SMOODEV pearl th-8078dd).

  Brings the .NET reference server (`SmooAI.SmoothOperator.Server`) to behavioral parity with the Rust server's OTP / session-identity seam (PR #132), so a public agent's `end_user`-gated tools can offer a one-time-code identity flow while the server stays credential-free.

  - New host seam `IOtpService` (`SendOtpAsync(sessionId, contact) -> OtpDelivery`; `VerifyOtpAsync(sessionId, code) -> OtpVerifyOutcome.Verified | Invalid`) with the `OtpChannel` / `OtpContact` / `OtpDelivery` / `OtpError` value types. Registered via DI; absent ⇒ unchanged (the `end_user` gate fail-closed-refuses and no OTP is offered).
  - When a turn's auth gate refuses an `end_user` tool on an unverified session, an `IOtpService` is installed, and the session has a contact, the server emits `otp_verification_required`, calls `SendOtpAsync`, and emits `otp_sent` — before the terminal response. Admin refusals are never offered OTP.
  - New `verify_otp` action: a `Verified` outcome marks the session identity-verified (`otp_verified`); an `Invalid` outcome emits `otp_invalid` with the host's remaining-attempt count. Validation order mirrors Rust (requestId → sessionId → code → session-exists → service); no service installed ⇒ fail closed (`otp_invalid` / `NOT_FOUND`).
  - Per-conversation verified state is persisted in the session store and threaded into the auth gate via a store-backed `ISessionAuthenticator` default (replacing the hardcoded deny-all), so a verified caller's `end_user` tools run. The caller's email contact is captured at create-session time. Both are backed in the in-memory and Postgres stores with a shared contract test.

  The reference server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Event shapes validate against the same `spec/events/otp-*.schema.json`.

## 1.10.3

### Patch Changes

- f3ace72: Go server: OTP / session-identity seam parity for end-user tool auth (th-8078dd).

  Brings the Go reference server to parity with the Rust server's OTP / session-identity seam (PR #132). A public agent's `end_user`-gated tools can now offer a one-time-code identity flow, while the Go server stays credential-free — it never generates, delivers, or validates a code.

  - New `OtpService` seam (`SendOtp` / `VerifyOtp`) plus the `OtpContact`, `OtpDelivery`, `OtpChannel`, `OtpErrorCode`, and `OtpVerifyOutcome` value types, mirroring the existing resolver seams. Installed via `server.WithOtpService`; absent ⇒ unchanged fail-closed behavior (the gate refuses, no OTP offered).
  - The session's OTP-verified bit (`StoredSession.OtpVerified`, set by a successful `verify_otp`) is threaded into the auth gate so a verified caller's `end_user` tools run.
  - On an `end_user` refusal, with a service installed and a session contact captured at create-session time, the server emits `otp_verification_required`, calls `SendOtp`, and emits `otp_sent` (before the terminal `eventual_response`, matching the Rust ordering). `admin` refusals are never offered OTP.
  - New `verify_otp` action: validation order `requestId → sessionId → code → session-exists → no-service`; a correct code emits `otp_verified` and marks the session authenticated, a rejected code emits `otp_invalid` with the host's remaining attempts, and no installed service fails closed (`otp_invalid` / `NOT_FOUND`).

  Semantics match the Rust reference exactly. Exhaustive tests (seam types, verify_otp happy/invalid/no-service/unknown-session/missing-fields, offer-flow event order, admin-not-offered, verified-session-runs-tool); server events validate against the shared `spec/events/*` schemas.

## 1.10.2

### Patch Changes

- 8535264: Python server: OTP / session-identity seam parity with the Rust reference (SMOODEV pearl th-8078dd).

  Brings the Python operator server to behavioral parity with the Rust server's end-user OTP identity-verification seam (landed for Rust in #132). Like the reference, the Python server never generates, delivers, or validates a code — a new host seam, `OtpService` (`smooth_operator_server.otp`, with `OtpContact` / `OtpDelivery` / `OtpChannel` / `OtpError` / `OtpVerified` / `OtpInvalid`), owns generation, delivery, expiry, and attempt counting. Install one via `ServerState.otp_service` (or `FrameDispatcher(..., otp_service=...)`); absent (the default), behavior is unchanged — the `end_user` auth gate fail-closed-refuses and no OTP is offered.

  - When a turn's gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact (the caller's email, captured at create-session time), the server emits `otp_verification_required`, calls `send_otp`, and emits `otp_sent` — in that order, before the terminal `eventual_response`. An `admin` refusal is never OTP-remediable, so it is not offered.
  - A new `verify_otp` action validates a submitted code via `OtpService.verify_otp`: an `OtpVerified` outcome marks the session identity-verified (persisted on the session store) and emits `otp_verified`; an `OtpInvalid` outcome emits `otp_invalid` with the host's remaining-attempt count and optional machine-readable reason. Validation order mirrors Rust (requestId, sessionId, code required; unknown session → `SESSION_NOT_FOUND`; no service → fail closed `otp_invalid` / `NOT_FOUND`).
  - Per-session verified state is tracked on the session store and threaded into the tool auth gate as the resolved `session_authenticated` bit (the session's OTP-verified state OR'd with the existing `SessionAuthenticator` seam), so a verified caller's `end_user` tools run.

  The reference server does not park/auto-resume the original turn; the client re-sends after `otp_verified`. The four OTP event builders reproduce the shared conformance fixtures byte-for-byte; exhaustive tests cover verify happy/invalid/no-service/unknown-session/missing-field, the offer flow's emission order, admin-not-offered, no-contact/no-service/send-failure edges, and a verified session running the gated tool.

## 1.10.1

### Patch Changes

- 9352c87: TS server: OTP / session-identity seam parity with the Rust reference (pearl th-8078dd).

  Brings `typescript/server` to parity with the Rust server's end-user OTP / session-identity seam (#132). The native TS server can now offer a one-time-code identity-verification flow behind a public agent's `end_user` tool auth gate, without holding any credentials itself.

  - New host seam `OtpService` (`typescript/server/src/otp.ts`) with `sendOtp` / `verifyOtp`, mirroring the shape of the server's other pluggable seams (`AgentConfigResolver`, `SessionAuthenticator`). Installed via the `otpService` server option; absent → unchanged fail-closed behavior (the `end_user` gate refuses and no OTP is offered). The server never generates, delivers, or validates a code — the host owns generation, delivery, expiry, and attempt counting.
  - When a turn's auth gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact, the server emits `otp_verification_required`, calls `sendOtp`, and emits `otp_sent` — in that order, before the terminal `eventual_response`.
  - New `verify_otp` action validates a submitted code: a `verified` outcome marks the session identity-verified and emits `otp_verified`; a non-verified outcome emits `otp_invalid` with the host's remaining-attempt count. No service installed → fail closed (`otp_invalid` / `NOT_FOUND`).
  - The session's OTP-verified bit is tracked on the session store (`contactEmail` captured at create-session time, `otpVerified` set by `verify_otp`) and threaded into the `end_user` auth gate, so a verified caller's gated tools run on the re-sent message. Admin refusals are never offered OTP.

  The server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Four protocol event builders + the shared `spec/conformance/fixtures.json` OTP fixtures + exhaustive tests (verify_otp happy/invalid/no-service/unknown-session/missing-fields, offer-flow event order, admin-not-offered, verified-session tool execution) added.

## 1.10.0

### Minor Changes

- 86d9e4f: Server-side OTP / session-identity seam so hosts can wire end-user tool auth (SMOODEV pearl th-8e8a89).

  The Rust reference server can now offer a one-time-code identity-verification flow behind a public agent's `end_user` tool auth gate, without holding any credentials itself. A new host seam, `OtpService` (`smooth_operator::otp`), owns code generation, delivery, expiry, and attempt counting; the reference server only orchestrates the wire flow around it. Install one via `AppState::with_otp_service`; absent, behavior is unchanged (the `end_user` gate fail-closed-refuses and no OTP is offered).

  - When a turn's auth gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact, the server emits `otp_verification_required`, calls `send_otp`, and emits `otp_sent`.
  - A new `verify_otp` action validates a submitted code via `OtpService::verify_otp`: a `Verified` outcome marks the session identity-verified and emits `otp_verified`; an `Invalid` outcome emits `otp_invalid` with the host's remaining-attempt count. With no service installed, verification fails closed (`otp_invalid` / `NOT_FOUND`).
  - Per-session verified state is tracked in session metadata and threaded into the auth gate as the real `session_authenticated` bit (previously hardcoded `false`), so a verified caller's `end_user` tools run.

  The reference server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Rust-only for now (mirrors how per-agent config landed as separate per-language PRs); parity in the Python/TS/Go/.NET servers is follow-up work.

## 1.9.0

### Minor Changes

- 0e29a9b: Per-agent behavior config: honor `instructions` + run `conversation_workflow` (SMOODEV-590).

  The reference server resolved a turn's system prompt from **per-org** settings, so every agent in an org spoke with the same voice and `conversation_workflow` was never applied — a public chat agent ignored its own persona and behaved as the generic customer-support bot.

  Config-delivery seam (matches the sibling Python/TS/C#/Go lanes): `AgentConfigResolver::resolve(agent_id)` — the ws protocol's `create_conversation_session` carries only an agent UUID, so config is resolved **server-side by id**. Default `StaticAgentConfigResolver` (empty ⇒ no-op, behavior unchanged); a `PgAgentConfigResolver` reads the monorepo `agents` table on the adapter's existing pool. The runner now:

  - uses the agent's `instructions` (+ `personality.persona`) as the system prompt, overriding the org default;
  - injects the agent's `greeting` into the prompt only on the first turn of a conversation;
  - restricts the turn's tools to `tool_config.enabledTools` (`enabled == true` entries by snake_case `toolId`; empty/absent ⇒ full set; unknown ids ignored), and delivers each entry's `config` to the tool via `ToolProviderContext`;
  - enforces per-tool `authLevel` at execution against the agent's `visibility` (a `ToolHook` gate: admin blocked on public agents; internal auto-satisfies; end_user on public requires an identity-verified session, fail-closed — the OTP flow is a host seam);
  - when a `conversation_workflow` is set, injects the current step's intent/criteria and, after each turn, runs a cheap failure-tolerant judge on the configurable `judge_model` (haiku-tier default) to advance the step; the step id is tracked per session.

  Per-agent isolation, malformed-jsonb tolerance (degrade to org default, never crash the turn), judge-failure tolerance (stay on the current step), and the authLevel branches (admin/end_user/internal, authed vs not) are covered by unit + integration tests.

- 9db9007: C# server: honor per-agent config + implement conversation workflows. An agent's `instructions.prompt` now drives its system prompt (overriding the org/default persona), so agents in the same org behave as themselves rather than a generic customer-support persona. `conversation_workflow` (goal + intent/criteria steps) is now implemented as a stepped, judge-advanced guided-agency flow: the current step's intent + criteria are rendered into the system prompt, and a cheap post-turn judge decides whether the step's criteria were met to advance (explicit `next` or sequential), with the current step id persisted per conversation. Per-agent `greeting` is woven into the agent's first reply only (first-turn prompt seed), and `tool_config.enabledTools` restricts the server's tool set to the agent's enabled snake_case toolIds per turn (empty/absent ⇒ the full set, unchanged). At tool-execution time each entry's `authLevel` is enforced (admin blocked on public agents; `end_user` needs a verified session via the new `ISessionAuthenticator` seam, default fail-closed; internal agents auto-satisfied; only tools declaring `supportsAuthRequirement` are gated) and its per-tool `config` is delivered to the executing tool. The workflow judge model is the uniform `judgeModel` option. Per-agent config reaches the server through a new `IAgentConfigResolver` DI seam (`ResolveAsync(agentId)`, default dict-backed `StaticAgentConfigResolver`) — `create_conversation_session` carries only an agent UUID, so config is resolved server-side per turn from the session's agent (mirroring the TS / Python lanes' `AgentConfigResolver`). jsonb parsing is tolerant (malformed config degrades to the default persona, never crashes a session) and the judge is failure-tolerant (any error keeps the conversation on the current step). Mirrors the Rust server change and the monorepo SMOODEV-590 behavior.
- a69a799: C# server local flavor: serve a prebuilt SPA same-origin from `SMOOTH_WEB_DIR` with the local token injected into `index.html` as `window.__SMOOTH_TOKEN__`, a `SMOOTH_LOCAL_TOKEN` → `LocalTokenVerifier` for same-origin `/ws` auth, and `SMOOTH_PERSONA` to set the agent's system prompt. Lets the .NET server be a drop-in "Big Smooth" backend behind the shared smooth-web Presence UI (validated end-to-end: SPA + WS + streamed persona reply).
- a6fab4a: Go server: honor per-agent config + implement conversation workflows (SMOODEV-590).

  Agents served by the Go operator now respect their own per-agent config instead of all sharing one generic org persona. A new `AgentConfigResolver` seam resolves a session's `agentId` into its `AgentConfig` (instructions, `Workflow`, greeting, personality, tool allow-list); resolution is server-side because the `create_conversation_session` payload carries only an `agentId`. An un-configured agent (no resolver, or resolver returns nil) falls back to the server/org default prompt + full tool set, so existing behavior is unchanged. Wire one in via `server.WithAgentConfigResolver`.

  `conversationWorkflow` is implemented as a stepped, judge-advanced guided-agency flow: the current step's intent + criteria are rendered into the system prompt (`<ConversationWorkflow>` block), and after each turn a cheap failure-tolerant judge LLM call decides whether the criteria were met and advances the pointer (following `next` or array order), tracked as `CurrentStepID` on the session. Malformed config degrades to the default flow and never crashes a session. Mirrors the TS/Python server siblings and the Rust reference's `agent-config-instructions-workflow` design.

- ebd2ad2: Python server: honor per-agent config + implement conversation workflows (SMOODEV-590).

  Agents served by the Python server previously ignored their per-agent config and always used the generic server-wide "customer support agent" persona. Now:

  - **Per-agent `instructions`** drive the system prompt for that agent's conversations, overriding the server-wide default (falling back to it when unset). Per-agent `personality` and first-turn `greeting` are plumbed into the prompt; `tool_config.enabledTools` (`[{ toolId, enabled, authLevel, config }]`, the monorepo `AgentToolConfig` shape) is a tool allow-list restricting the agent's turns to the `enabled=true` tools by `toolId` (empty/absent → full set; unknown toolIds ignored), matching the Go/TS lanes. Per-tool `authLevel` is enforced at execution against the agent's `visibility` and a `SessionAuthenticator` seam (admin blocked on public agents; internal auto-satisfies; end_user on public requires identity verification, fail-closed), and each entry's `config` is delivered to the tool at execution. The post-turn judge model is a `judge_model` server option (haiku-tier default).
  - **`conversation_workflow`** is implemented as a stepped, judge-advanced guided flow: the current step's intent + criteria are rendered into the system prompt, and a cheap post-turn judge call decides whether the criteria were met and advances to the next step (explicit `next` → sequential → terminal). The current step id is tracked per conversation.

  Config parsing is tolerant — a malformed workflow or config degrades to the server default and never crashes a session. The judge is failure-tolerant — any judge error leaves the conversation on the current step. Delivery seam: `ServerState.agent_config_resolver` (`AgentConfigResolver.resolve(agentId)`, default dict-backed `StaticAgentConfigResolver`) is resolved per turn from the session's agent — the ws protocol carries only an agent UUID, so config is looked up server-side. Empty resolver → behavior unchanged. Mirrors the Rust reference PR.

## 1.8.0

### Minor Changes

- 023c531: feat(auth): JWKS-based JWT verification (ES256 + any algorithm, with rotation) for `smoo`/`jwt` modes

  The auth verifier could only validate tokens against a **static RS256 PEM**
  (`AUTH_JWT_RS256_PUBLIC_KEY`). SmooAI's `auth.smoo.ai` (the `smoo` issuer) signs
  dashboard tokens with **ES256** (`/.well-known/jwks.json` → `alg: ES256, kty: EC`),
  so every real SmooAI token was rejected — blocking `AUTH_MODE=smoo` for the SmooAI
  K8s flavor.

  This adds a JWKS-backed verification path (additive, behavior-preserving):

  - New optional `AUTH_JWT_JWKS_URL`, and auto-derivation of
    `{AUTH_JWT_ISSUER}/.well-known/jwks.json` when an issuer is set and no static
    key is given.
  - Keys are fetched, **cached** (TTL) and **rotation-aware** (refresh-on-unknown-`kid`),
    selected per-token by `kid`, and validated with the key's algorithm via
    `DecodingKey::from_jwk` — so **any** advertised JWS algorithm works
    (ES256/ES384/RS256/PS256/EdDSA/…), not just RS256.
  - Wired into both `SmooIdentityVerifier` (the `smoo` path) and `JwtVerifier`
    (BYO), so any OIDC issuer works. `AuthVerifier::verify` stays **synchronous**
    (the keyset is read from cache; the network fetch is off the hot path).

  Key-source precedence (`jwt`/`smoo`): static `AUTH_JWT_RS256_PUBLIC_KEY` →
  static `AUTH_JWT_HS256_SECRET` → JWKS (`AUTH_JWT_JWKS_URL`, else issuer-derived).
  The static-RS256/HS256 paths are unchanged. With this, `AUTH_MODE=smoo` needs
  only `AUTH_JWT_ISSUER` (+ optional audience) — no static public key.

## 1.7.1

### Patch Changes

- 86dd6f8: local flavor: serve the canonical `@smooai/chat-widget` (Aurora Glass) bundle

  The local-flavor server now vendors and serves the published **`@smooai/chat-widget`**
  (Aurora Glass) standalone bundle instead of a parallel copy of the widget. One canonical
  public widget, consumed — not two. Same `<smooth-agent-chat>` element + `endpoint`/`agent-id`
  attributes, so it's a drop-in for the host page.

## 1.7.0

### Minor Changes

- 1d9c60e: feat: thread `organization_id` into `AccessContext` for per-turn knowledge scoping

  `StorageAdapter::knowledge_for_access(&self, access)` carried only `user_id` +
  `groups` — no org — so a multi-tenant relational backend (SmooAI) could not scope
  RAG to the turn's organization and was forced to a single static org. This was the
  last multi-tenant gap on the knowledge path.

  `AccessContext` now carries an additive `organization_id: Option<String>`
  (default `None`, set via the new `with_organization_id(...)` builder). The
  authenticated-principal path (`Principal::access_context`) stamps the principal's
  org automatically; the reference server / lambda send-message paths fall back to
  the turn's **session** org (every session carries `organization_id` since the
  create-session path derives it) when the requester has no org of its own. The org
  is then **available** to a host adapter's `knowledge_for_access` so it can scope
  retrieval to the right tenant.

  The operator's built-in single-tenant ACL ignores the org (org isolation already
  happens upstream), so this is behavior-preserving for the reference/local flavor.
  The Postgres knowledge adapter additionally uses the context's org — when present
  — to **override** its construction-time org as a cheap SQL pre-filter
  (`organization_id = $1`), so one adapter instance can serve per-turn tenants
  instead of being pinned to a single static org; an org-less context leaves the
  construction-time org unchanged.

## 1.6.0

### Minor Changes

- bdbf868: feat(server): derive org + agent from auth in `create_conversation_session`

  `handle_create_session` no longer hard-codes the seed org. It now derives the
  session's `organization_id` from the authenticated request, in priority order:

  1. the agent's widget-auth policy `organization_id` (widget visitors authenticate
     via origin + `authContext`, not a JWT, so their org rides on the agent policy —
     new optional `AgentWidgetAuth.organization_id` field),
  2. the connection's authenticated JWT principal org (dashboard / authed clients —
     the principal's `org_id` is now threaded from the `/ws` handshake through to the
     handler instead of being dropped at `AccessContext` reduction),
  3. the server's seed org as a behavior-preserving fallback for the no-auth/local
     flavor.

  The agent id continues to come from the inbound `agentId` payload. The same
  JWT-org-then-configured-org derivation is applied to the lambda dispatch
  create-session path. All existing in-memory/seed flows are unchanged.

## 1.5.0

### Minor Changes

- f2ecef9: Add `organizationId` to the `Session` domain type so org-scoping is uniform across every core domain type (`Conversation`, `Participant`, and `Message` already carry it). Storage backends can now write the session's org directly instead of re-deriving it from the conversation. The built-in Postgres adapter gains an `organization_id` column (additive, `DEFAULT ''`) on `conversation_sessions` plus an org index; the in-memory and DynamoDB adapters thread the new field through automatically; server/runner create-session paths populate it from the conversation/turn org already in scope.

## 1.4.0

### Minor Changes

- 45fd77e: Thread the turn's `conversation_id` and resolved per-org `gateway_key` into `ToolProviderContext`.

  A host's injected `ToolProvider` now receives the conversation the turn runs in and the LLM-gateway key that turn was billed/scoped to — alongside the existing `org_id` + `access`. This lets SmooAI's conversation-persisting tools correlate to the right conversation (instead of degrading to a no-op on an empty conversation id) and lets agent-brain's `knowledge_search` obtain the gateway key.

  Purely additive and behavior-preserving: both new fields are `Option`, default to `None` via `ToolProviderContext::new`, and existing `ToolProvider` impls that ignore them are unaffected. New builders `with_conversation_id` / `with_gateway_key` set them; the runner populates both from the turn it already has in hand.

## 1.3.0

### Minor Changes

- 12d348a: Add two host provider-injection seams to the chat runner so a deployment flavor can run a turn with its OWN tools and persona without forking the runner:

  - **Custom tool injection** — a new `ToolProvider` trait (`tools_for(&ToolProviderContext) -> Vec<Arc<dyn Tool>>`) plus `AppState::with_tools(provider)`. When installed, the runner merges the provider's per-turn tools into the turn's `ToolRegistry` alongside the built-ins; the `ToolProviderContext` carries the turn's `org_id` + `AccessContext` so a host can return per-org tools. No provider ⇒ the registry is exactly today's built-ins.
  - **Per-org agent persona** — an optional `AgentSettings.persona: Option<String>`; the runner uses the resolved persona as the turn's system prompt when present, else falls back to the existing const `KNOWLEDGE_CHAT_SYSTEM_PROMPT`. No persona ⇒ identical prompt to today.

  Both seams are behavior-preserving by default — the local/default flavor is unaffected.

- ab1aa9d: feat(server): `confirm_tool_action` — write-confirmation human-in-the-loop pause/resume

  The reference WebSocket server can now gate write tools behind human approval.
  When an agent turn calls a tool whose name matches `SMOOTH_AGENT_CONFIRM_TOOLS`
  (comma-separated substrings), the turn **parks** and emits a
  `write_confirmation_required` event (matching
  `spec/events/write-confirmation-required.schema.json`) carrying
  `{ toolId, actionDescription }`. The client resumes it by sending
  `confirm_tool_action` (`{ sessionId, requestId, approved }`, per
  `spec/actions/confirm-tool-action.schema.json`): on `approved: true` the parked
  tool executes; on `false` it is skipped with a rejection result the model sees,
  and the turn still completes.

  Built entirely on the existing smooth-operator-core human-gate primitive
  (`ConfirmationHook` + `human_channel()` + `AgentConfig::with_human_channel`) —
  **no core change required**. The server wires the hook's `HumanRequest` stream to
  a WS event and bridges an inbound `confirm_tool_action` back to the hook's
  `HumanResponse`, keyed by session. The `send_message` turn now runs in a spawned
  task so the socket reader stays free to receive the confirmation on the same
  connection (the turn would otherwise deadlock awaiting a frame it is blocking).

  With `SMOOTH_AGENT_CONFIRM_TOOLS` unset (the default), no `ConfirmationHook` is
  installed, no tool ever parks, and behavior is byte-for-byte unchanged. The
  local/default flavor is unaffected.

- feec0b5: Add a per-org LLM gateway-key resolution seam so a multi-tenant flavor can
  bill/scope each org's turns to its own gateway key (e.g. a per-tenant LiteLLM
  virtual key), while the local/default flavor keeps using the single environment
  key.

  - New `GatewayKeyResolver` trait (`smooth_operator::gateway_key`) — the public,
    contributable hook: `async fn resolve(&self, org_id: &str) -> Option<String>`.
  - Default `EnvGatewayKeyResolver` returns the single `SMOOAI_GATEWAY_KEY` for
    every org, so behavior is unchanged unless a host injects a per-org resolver.
  - `resolve_gateway_key(resolver, org_id, env_key)` helper centralizes the
    resolve-then-fall-back-to-env contract used by the per-turn LLM-config build.
  - The server's `AppState` holds an `Arc<dyn GatewayKeyResolver>` (default =
    `EnvGatewayKeyResolver`) with a `with_gateway_key_resolver(...)` builder for
    injection. `send_message` resolves the turn's `org_id` from its conversation,
    resolves the key, and falls back to the env key when the resolver returns
    `None`.

  Behavior-preserving by default: with no resolver injected, every turn uses the
  env key exactly as before. No SmooAI/DB specifics live in the shared code — only
  the trait and the env default; a host injects its own per-org key store.

- 45be211: Add a `get_conversation_messages` WebSocket action to `smooth-operator-server`. Returns paginated message history for a session's conversation (`{ conversationId, messages, nextCursor, hasMore }`), wrapping the existing `StorageAdapter::list_messages_by_conversation` (the same call the admin API + turn runner use). Optional `limit` (default 50) + opaque `cursor`, newest-first. Completes wire-compat for chat clients that page history over the socket (previously only `/admin` exposed it).
- cf6fab4: feat(server): graceful SIGTERM/ctrl_c drain of WebSocket connections.

  The reference WebSocket server (`smooth-operator-server`) now drains in-flight
  turns on shutdown instead of being killed mid-flight. Previously `run()` did a
  plain `axum::serve(listener, app).await` with no `with_graceful_shutdown`, so on
  a Kubernetes pod termination (scale-down / rollout) the process was killed while
  turns were in progress — in-flight WebSocket turns dropped and connections never
  `detach`ed from the `Backplane`, leaving stale registry entries in Valkey/NATS.

  A single shared `tokio_util::sync::CancellationToken` is now threaded through
  `AppState` (`shutdown`, defaulted to a fresh never-cancelled token in
  `AppState::new`, plus a `with_shutdown` builder). Each per-connection reader loop
  `select!`s on that token (`biased`, shutdown wins ties) with the inbound-frame
  read — and keeps `handle_frame(...).await` inside the frame arm so a turn already
  in flight finishes before the next shutdown check. After the loop the existing
  `backplane.detach(...)` runs, so the connection always leaves the registry clean.
  The serve loop (`run`) wires `axum::serve(...).with_graceful_shutdown(...)` to
  SIGTERM (k8s) or ctrl_c (interactive), cancelling the token to fan the drain out
  to every connection within the chart's `terminationGracePeriodSeconds` window.

### Patch Changes

- 7545ea8: Add an unauthenticated `GET /health` HTTP route to `smooth-operator-server`. A WebSocket `/ws` upgrade can't answer a plain GET healthcheck, so HTTP load balancers (AWS ALB, nginx ingress) had nothing to probe; `GET /health` now returns `200 OK`, dependency-free (no storage/LLM touch). Enables HTTP health checks for the K8s deployment flavor.

## 1.2.0

### Minor Changes

- 5971864: Phase 4: streaming turn execution across the Python, TypeScript, and Go cores (C#
  already streams via MEAI's `RunStreamingAsync`). A new streaming run method alongside
  the existing `run()` — TS `runStream` (`AsyncGenerator<StreamEvent>`), Python
  `run_stream` (`AsyncIterator[StreamEvent]`), Go `RunStream` (returns a `*Stream` whose
  `Events()` channel carries `StreamEvent`s and whose `Err()` reports a mid-turn model
  error) — drives the SAME agentic loop (system/knowledge/memory build, compaction, cost
  tracking, budget early-stop, deferred tools, clearance + human-gate, checkpoint/thread
  persistence) but calls the model in STREAMING mode and yields incremental events: a
  `text` event per content delta, a `tool_call` event per requested call (before
  dispatch), a `tool_result` event per finished tool (in original call order even under
  `parallelToolCalls`), and exactly one terminal `done` event carrying the same
  `AgentRunResponse` `run()` would return. The provider seam gains an OpenAI-style
  streaming call (`createStream` / `create(..., stream=True)` / `ChatStream`) that
  accumulates content + `tool_calls` deltas by index into a full assistant message, so
  the rest of the loop is unchanged; usage is read from the final chunk for cost/budget.
  The reusable mock LLM providers replay their FIFO script as chunked deltas (text split
  into pieces, tool-call arguments split across two chunks). Retry-with-backoff is
  intentionally not applied to streaming (re-running would re-emit chunks), mirroring C#.

## 1.1.0

### Minor Changes

- a89045d: Phase 4: concurrent (parallel) tool-call execution across the Python, TypeScript, Go,
  and C# cores. A new opt-in `parallelToolCalls` option (Python `parallel_tool_calls`,
  Go/C# `ParallelToolCalls`), default false, dispatches an assistant turn's tool calls
  concurrently (`asyncio.gather` / `Promise.all` / goroutines + `sync.WaitGroup` /
  `Task.WhenAll`) when there are two or more. The tool-result messages are still appended
  in the original tool-call order, so the transcript stays deterministic regardless of
  completion order; a failing or human-denied tool keeps its error result in its correct
  position. With the flag off (the default) — or for single-tool-call turns — dispatch is
  unchanged from today's sequential behavior. Per-tool semantics (clearance, human-gate
  approval, tool_search promotion, JSON-arg parsing) are untouched.

## 1.0.0

### Major Changes

- 6f6f622: Unified 1.0.0 polyglot publish — all five language implementations now ship from one changeset at one shared version via the existing lockstep release.

  - **Rust** reclaims the crate name `smooai-smooth-operator` (the predecessor standalone engine 0.13.x is superseded by `smooai-smooth-operator-core`) and publishes the full set: the reference lib plus 7 library crates (`-ingestion`, the `-adapter-*` storage/backplane adapters, and `-server`) to crates.io.
  - **Python** distributions are renamed to `smooai-smooth-operator` and `smooai-smooth-operator-core` (PyPI), keeping the `smooth_operator` / `smooth_operator_core` import packages unchanged.
  - **Go** is published by tag `go/v1.0.0` (subdir module `github.com/SmooAI/smooth-operator/go`).
  - **npm** (`@smooai/smooth-operator`) and **NuGet** (`SmooAI.SmoothOperator.Core`) continue as before.

  One changeset → one shared version → npm + NuGet + crates.io + PyPI + Go tag, all stamped by `scripts/sync-versions.mjs`.

## 0.9.0

### Minor Changes

- 08f1780: Phase 2: human-in-the-loop approval (HumanGate) across the Python, TypeScript, and
  Go cores, at parity with the C# reference. The agent consults an optional approval
  gate before running any tool flagged by a `requires_approval` predicate; a denial is
  fed back to the model as the tool result (the tool never runs) and an approval lets
  it execute normally. With no gate configured, behavior is unchanged.

## 0.8.0

### Minor Changes

- a8bfb62: HTTP-backed widget auth (SMOODEV-1890): `HttpWidgetAuth`, a generic `WidgetAuthProvider` that resolves each agent's embed policy (`allowed_origins` + `public_key`) by GETting `{base_url}/{agentId}` from a host policy service, with TTL caching. Response handling fails safe: 2xx caches the policy, 404 caches a no-policy result (denied under `WIDGET_AUTH_STRICT`), and 5xx/network/malformed responses return `None` without caching so the next connect retries. The server now installs it from env — set `WIDGET_AUTH_URL` (plus optional `WIDGET_AUTH_BEARER` / `WIDGET_AUTH_TTL_SECS`) to enforce embeddable-widget auth against a host's policy service with no custom binary; unset leaves the permissive default. This is the reusable mechanism a host backs with its own agent store (SmooAI points it at an api-prime route).
- bc901d7: Persistent + semantic agent memory (SMOODEV-1470, parity gap Phase 3): `PgMemory`, a pgvector-backed implementation of the core `Memory` trait in the `adapters/postgres` crate. Before this the only `Memory` backend was the core `InMemoryMemory` (a `Vec` behind a `Mutex`, keyword recall, lost on restart). `PgMemory` gives the general agent cross-thread user memory that survives restarts and recalls by semantic similarity — the Rust equivalent of the TS `store`/`store_vectors` namespaced by `['memories', orgId, userId]`.

  Each `PgMemory` instance is bound to one `(organization_id, user_id)` namespace at construction (built via `PostgresAdapter::memory(org, user)`; `user_id = None` for org-wide memory), mirroring how `PgKnowledgeBase` binds an org — the core `Memory::recall(query, limit)` signature carries no scoping, so scoping is threaded through the constructor. `store` embeds the entry content and upserts a row in a new `memories` table (`embedding vector(N)` matching the active `Embedder` dim, HNSW cosine index, namespaced by `(organization_id, user_id)`); `recall` embeds the query and returns the namespace's top-K by pgvector cosine distance with `relevance` set to the cosine similarity; `forget` deletes within the namespace. Embedding goes through the shared `Embedder` seam (DeterministicEmbedder offline, GatewayEmbedder live), so memory and knowledge vectors share column width and hashing. Covered by a testcontainers integration test (semantic recall, org/user namespace isolation, namespace-scoped forget, empty recall) that skips cleanly when Docker is unavailable. No change to the core `Memory` trait was required.

## 0.7.0

### Minor Changes

- ed12900: Realtime publish endpoint (SMOODEV-1893): `POST /admin/publish` lets non-AI publishers — job status, ingestion progress, notifications, billing — push an event to a backplane target over the WebSocket fleet without going through an agent turn. Body is `{ target: { type: session|user|org|agent|connection, id }, event }`; it calls `Backplane::publish`, so with a distributed backplane the event fans out across pods. Admin-gated (RBAC role 2); the response reports local deliveries on the serving pod (cross-pod deliveries happen but aren't counted). Targets are opaque ids matched against the connection registry — tenant id-namespacing is a host concern, documented on the handler.

## 0.6.0

### Minor Changes

- e9fa854: Distributed Backplane backends (SMOODEV-1892): `RedisBackplane` and `NatsBackplane` — the horizontal scale-out seam. Both implement the `Backplane` trait by wrapping a per-pod `InMemoryBackplane` for local registry + delivery and adding a pub/sub bus (Redis/Valkey channel or NATS subject) for cross-pod fan-out: `publish(Target, event)` delivers to local sockets immediately, then broadcasts a `BackplaneEnvelope` so every other pod re-resolves the target against its own registry and delivers to its sockets (the origin pod skips its own echo). This makes the same `publish` call reach a socket on any replica — required to run the WS service with >1 pod, and the cross-pod path for non-AI publishers. Selected at runtime via `SMOOTH_AGENT_BACKPLANE` (`memory` | `redis`/`valkey` | `nats`) + `SMOOTH_AGENT_BACKPLANE_URL`; default stays single-process in-memory. `Target` is now `Serialize`/`Deserialize` and a shared `BackplaneEnvelope` is exposed so a host's own transport adapter can speak the same wire format. New crates: `adapters/backplane-redis`, `adapters/backplane-nats` (cross-pod fan-out proven end-to-end over real Redis + NATS via testcontainers).

## 0.5.0

### Minor Changes

- e6d9dbe: Connection backplane (SMOODEV-1891): a pluggable `Backplane` trait + default `InMemoryBackplane` in the OSS server — the scale-out + event-delivery seam. Each connection's outbound sink is attached on connect and associated with its session/agent; `publish(Target, event)` delivers to every connection for a target. This is the foundation for running >1 replica (a Redis/NATS impl makes delivery cross-pod) and the plug point for non-AI realtime: any service can `publish(Target::Session(...), event)` and reach the connected client over WebSocket. Wired into `AppState` (`with_backplane`) + the connection lifecycle. Runtime-agnostic (the sink is a closure, no tokio dep added to the lib).

## 0.4.0

### Minor Changes

- 715f79c: Embeddable-widget auth (SMOODEV-1878): a pluggable `WidgetAuthProvider` hook in the Rust server that enforces a per-agent **origin allowlist** + public-key **`authContext`** (HMAC-SHA256, replay-protected) for `<smooth-agent-chat>` connections. The `Origin` header is captured at the WebSocket handshake and validated at `create_conversation_session`; hosts plug in a concrete provider (backed by their agent store) while the bundled `PermissiveWidgetAuth` leaves a standalone OSS server unaffected. `WIDGET_AUTH_STRICT=1` fails closed on unknown agents.

## 0.3.0

### Minor Changes

- 0933942: C# server (`SmooAI.SmoothOperator.Server`) + engine hardening, at Rust parity.

  Server (new):

  - Durable Postgres adapters: ACL knowledge store (ACL filtered in SQL via `acl_groups && groups`, leak contract on both in-memory and Postgres backends), session store, and checkpoint store — agent state, sessions, and ACL-scoped knowledge all survive a restart.
  - `GatewayEmbedder` for real semantic retrieval (deterministic fallback when no gateway key).
  - Reranker: opt-in post-retrieval reorder (`SMOOTH_AGENT_RERANK=gateway|lexical|off`) — engine `IReranker`/`NoopReranker`/`LexicalReranker` + server `GatewayReranker` + `RerankSelection`, wired through the turn; fails soft if the reranker errors.
  - Auth-gated `/admin` API: `/admin/health`, `/admin/me`, `/admin/connectors`, and `POST /admin/reindex` (re-ingest without a restart); fail-closed Bearer auth.
  - Tool `stream_chunk`s: tool call/result surfaced over the WebSocket protocol.
  - Deployable host (`SmooAI.SmoothOperator.Server.Host`) + Dockerfile: wires gateway model, storage, JWT/trusted/none auth, and startup GitHub ingestion.

  Engine (`SmooAI.SmoothOperator.Core`):

  - `IReranker` + `NoopReranker` + `LexicalReranker` + `Rerankers.ApplyOptionalAsync`.
  - `RunStreamingAsync` now yields the tool-result update so tool results surface in the stream.

  Robustness fixes:

  - Chunker no longer infinite-loops on long non-whitespace runs (minified code / base64 / long URLs).
  - The dispatcher emits a clean error and keeps the connection alive on any handler exception (was dropping the socket silently).
  - Postgres checkpoint store preserves tool-call/result content (was serializing text only).
  - GitHub connector fails loud on a truncated tree instead of silently indexing a partial repo.
