---
"@smooai/smooth-extension-sdk": minor
"@smooai/smooth-operator": patch
---

SEP Phase 3 (SDK + spec) — the `ui/request` surface.

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
