# Vendored widget assets (local deployment flavor)

`chat-widget.iife.js` is the prebuilt standalone bundle of the **`@smooai/chat-widget`**
npm package — the canonical public **Aurora Glass** widget (the `<smooth-agent-chat>`
web component, `dist/chat-widget.global.js`, with the smooth-operator protocol client
inlined). It is vendored here so the local deployment flavor can serve the official
widget **offline**, with no Node build step.

> Why `@smooai/chat-widget` and not `@smooai/smooth-operator/widget`: `@smooai/chat-widget`
> is the single canonical public widget (Aurora Glass redesign + OTP/HITL UI). The
> `@smooai/smooth-operator` SDK previously shipped a parallel copy of the same web
> component; we consume the published Aurora Glass package directly so there is one
> widget, not two. Same `<smooth-agent-chat>` element + `endpoint`/`agent-id` attributes,
> so it's a drop-in.

`widget-index.html` is the host page the local flavor serves at `/`; it loads
the bundle and points a `<smooth-agent-chat>` at this server's own `/ws`, with
the auth token injected into the `?token=` slot (same-origin, loopback).

These are served **only** when a host opts in via
`LocalServerBuilder::serve_widget(...)` — the K8s/Lambda flavors never mount the
widget routes.

## Keeping it current

Pinned to `@smooai/chat-widget@0.5.0`. To refresh after a widget release:

```sh
npm pack @smooai/chat-widget
tar xzf smooai-chat-widget-*.tgz -C /tmp package/dist/chat-widget.global.js
cp /tmp/package/dist/chat-widget.global.js chat-widget.iife.js
```

(A CI step that does this on `@smooai/chat-widget` release would keep the two in lockstep.)
