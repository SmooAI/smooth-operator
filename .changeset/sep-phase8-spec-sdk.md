---
'@smooai/smooth-operator': minor
'@smooai/smooth-extension-sdk': minor
'create-smooth-extension': minor
---

SEP Phase 8 (spec + SDK + demo) — long-tail pi parity.

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
