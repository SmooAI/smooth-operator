# Porting a pi extension to SEP (`@smooai/smooth-extension-sdk`)

This is the pi → SEP parity checklist and the end-state acceptance for the SEP
epic. Every member of pi's `ExtensionAPI`
(`pi/packages/coding-agent/src/core/extensions/types.ts`) maps to a smooth
equivalent, a documented **port delta**, or a documented **N/A** (with the
reason). SEP is JSON-RPC-over-stdio (the extension is a subprocess), so the two
structural deltas that touch almost everything are:

- **Session reads are awaited.** pi runs in-process and reads session state
  synchronously; over SEP a read is a request. In practice most pi extensions
  only *write* (send/append) and subscribe to events, so this rarely bites.
- **Function renderers become render blocks.** pi hands you a React/Ink
  component factory; SEP has no shared component runtime across five hosts, so
  UI is the declarative [`RenderBlock`](src/protocol.ts) DSL with a mandatory
  `text` fallback. The interactive tier is the `widget` kind + `widget/key`.

Legend: ✅ direct · 🔁 port delta · 🚫 N/A (with reason)

## Definition & lifecycle

| pi | SEP | |
|---|---|---|
| `defineExtension((pi) => …)` | `defineExtension((smooth) => …)` | ✅ |
| `pi.name` / `pi.version` | `smooth.name` / `smooth.version` | ✅ |
| in-process jiti load | subprocess + `initialize` handshake (`serve()`) | 🔁 spawn model; capabilities/trust negotiated at handshake |

## Events & hooks (`on`)

Observe events are fire-and-forget subscriptions; intercept hooks are awaited and
may `block`/`patch`. Both go through `smooth.on(name, handler)`.

| pi event/hook | SEP | |
|---|---|---|
| `agent_start` / `agent_end` | `on('agent_start' \| 'agent_end')` | ✅ observe |
| `turn_start` / `turn_end` | `on('turn_start' \| 'turn_end')` | ✅ observe |
| `message_start` / `message_update` / `message_end` | same names | ✅ (`message_end` is also a hook) |
| `tool_execution_start/update/end` | same names | ✅ observe |
| `model_select` | `on('model_select')` | ✅ observe |
| `tool_call` | `on('tool_call')` → `{block}` / `{patch}` | ✅ hook (fail-closed) |
| `tool_result` | `on('tool_result')` | ✅ hook (fail-open) |
| `user_bash` | `on('user_bash')` | ✅ hook (fail-closed) |
| `input` | `on('input')` | ✅ hook |
| `before_agent_start` | `on('before_agent_start')` → `{patch:{system_prompt}}` | ✅ hook (Phase 8 wired) |
| `context` | `on('context')` → `{patch:{messages:[{role,content}]}}` | ✅ hook (Phase 8 wired); 🔁 pi-friendly `{role,content}` shape, not the engine `Message` |
| `before_provider_request` | `on('before_provider_request')` | ✅ hook |
| `after_provider_response` | `on('after_provider_response')` | ✅ observe |
| `thinking_level_select` | — | 🚫 folded into `model_select` (`session/set_model` carries `thinking`) |
| `resources_discover` | `[resources]` manifest dirs (skills/prompts/themes) | 🔁 declarative discovery, not an event (host-driven, avoids the trust bootstrap paradox) |
| `project_trust` | host-internal trust engine | 🚫 extensions cannot participate in trust decisions (bootstrap paradox) |
| `session_start` / `session_shutdown` / `session_compact` | `on(...)` (host-level events) | ✅ where the host has a session (daemon/CLI); engine emits none |
| `session_before_fork` / `session_before_tree` / `session_tree` | `session_before_tree` hook + `session_tree` event defined | 🔁 **capability-gated, deferred**: wired only where a host surfaces a session tree; smooth-code has the `BranchableSession` model but it is dormant, so no live consumer yet |

## Tools

| pi | SEP | |
|---|---|---|
| `registerTool(...)` | `smooth.registerTool(defineTool({...}))` | ✅ schema (zod/TypeBox → JSON Schema), `execute`, streaming `ctx.onUpdate`, `ctx.signal` cancel |
| tool `promptSnippet` / `promptGuidelines` | tool `description` | 🔁 folded into description (host `tool_hints` TOML covers built-ins) |
| `getActiveTools()` / `setActiveTools()` | `tools/set_active` | ✅ clamped to the per-agent `enabled_tools` allow-list (never widens auth) |
| `getAllTools()` / `getCommands()` | — | 🚫 introspection reads not exposed (YAGNI; add if a port needs them) |
| deferred / `tool_search` tools | `deferred: true` on the registration | ✅ |

## Commands, flags, shortcuts

| pi | SEP | |
|---|---|---|
| `registerCommand(name, {...})` | `smooth.registerCommand(defineCommand({...}))` + autocomplete | ✅ command-tier context |
| `registerFlag(name, {...})` / `getFlag(name)` | `smooth.registerFlag({...})` / `smooth.getFlag(name)` | ✅ |
| `registerShortcut(...)` | `smooth.registerShortcut({key, command})` | ✅ TUI frontends honor it |

## Message rendering

| pi | SEP | |
|---|---|---|
| `registerMessageRenderer(customType, renderer)` | `smooth.registerMessageRenderer(tag, template)` | 🔁 **declarative** render-block template with `{{path}}` placeholders instead of a render function |

## Actions & session

| pi | SEP | |
|---|---|---|
| `sendMessage(msg, {deliverAs, triggerTurn})` | `ctx.session.sendMessage(text, {role})` | ✅ command-tier |
| `sendUserMessage(content, {deliverAs})` | `ctx.session.sendUserMessage(text, {deliverAs})` | ✅ steer/follow_up/next_turn |
| `appendEntry(customType, data)` | `ctx.session.appendEntry(entry)` | ✅ LLM-invisible entry |
| `setSessionName` / `getSessionName` / `setLabel` | — | 🚫 session-metadata; add per-host when a frontend surfaces it (deferred with session tree) |
| `exec(command, args, opts)` | `ctx`-less `exec/run` (audited host permission engine) | 🔁 the extension is itself a process, so `exec` routes through the host's audited path, not the extension's shell |

## Model & providers

| pi | SEP | |
|---|---|---|
| `setModel(model)` | `ctx.session.setModel(model, {provider, thinking})` | ✅ (Phase 7) |
| `getThinkingLevel` / `setThinkingLevel` | `setModel(..., {thinking})` | 🔁 thinking rides `set_model` |
| `registerProvider(name, config)` | `smooth.registerProvider(defineProvider({...}))` | ✅ declarative + OAuth round-trips + proxied streaming (Phase 7) |

## UI

| pi | SEP | |
|---|---|---|
| `select` / `confirm` / `input` | `ctx.ui.select/confirm/input` | ✅ |
| `notify` / `setStatus` / `setTitle` | `ctx.ui.notify/setStatus/setTitle` | ✅ |
| `setWidget` / overlays | `ctx.ui.setWidget(renderBlock)` | 🔁 declarative render blocks; interactive tier = `widget` kind + `widget/key` |
| `hasUI` | `ctx.hasUI(kind)` (per-kind, capability-negotiated) | ✅ |
| render assets (`render/web.js` web-component) | `[render] web = …` escape hatch | 🔁 **deferred**: the trust + CSP story needs the daemon epic's asset-serving surface |
| `setEditorComponent(factory)` | — | 🚫 no shared editor component runtime across five hosts |
| `onTerminalInput(handler)` | — | 🚫 raw terminal input is frontend-owned; use `widget` keybindings instead |
| custom Ink/React TUI components | render blocks | 🚫 no shared component runtime; the `RenderBlock` kinds + `text` fallback are the contract |

## Render blocks (pi function renderers → SEP DSL)

`render.markdown/keyvalue/table/diff/progress/stack/widget` build the wire
shapes. Every block carries an optional `text` fallback (frontends derive one
when omitted). The `widget` kind wraps a `body` block and declares
`keybindings`; the host routes matching keys back as `widget/key` events. See
[`examples/snake.ts`](examples/snake.ts) for the full interactive loop.

## Resources & packages

| pi | SEP | |
|---|---|---|
| `skillPaths` / `promptPaths` / `themePaths` | `[resources] skills/prompts/themes` in `extension.toml` | ✅ discovery unifies into smooth-cast (skills) / theme discovery (Phase 8) |
| extension packages (bundled tools+skills+mcp) | `extension.toml` + `plugin.toml` (zero-code) + bundled `mcp.toml` | 🔁 packaging is manifest-declared, installed via `th ext install npm:/git:` |

## Inter-extension bus (Phase 8)

| pi | SEP | |
|---|---|---|
| `pi.events.publish(topic, payload)` | `smooth.events.publish(topic, payload)` | ✅ |
| `pi.events.on(topic, handler)` | `smooth.events.on(topic, handler)` | ✅ (a filtered `bus/event` subscription) |

## Acceptance status

- **Direct or delta-with-equivalent:** every tool, command, flag, shortcut,
  provider, UI-dialog, action, hook, and observe event pi exposes.
- **Documented N/A:** `setEditorComponent`, `onTerminalInput`, custom TUI
  components, `project_trust`, `thinking_level_select` (folded), tool
  introspection reads.
- **Deferred (with reason):** session tree/fork events (dormant host seam), the
  web render-assets escape hatch (needs the daemon asset surface), session
  metadata setters (ride with the session-tree work).

A real pi extension (`todo`) ports with only the two documented mechanical
changes (awaited session reads; renderers → render blocks); `snake` is the
interactive-widget proof.
