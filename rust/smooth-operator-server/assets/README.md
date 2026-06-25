# Vendored widget assets (local deployment flavor)

`chat-widget.iife.js` is the prebuilt standalone widget bundle from the
published **`@smooai/smooth-operator`** npm package (the `<smooth-agent-chat>`
web component, `dist/widget/chat-widget.iife.js`). It is vendored here so the
local deployment flavor can serve the official widget **offline**, with no Node
build step.

`widget-index.html` is the host page the local flavor serves at `/`; it loads
the bundle and points a `<smooth-agent-chat>` at this server's own `/ws`, with
the auth token injected into the `?token=` slot (same-origin, loopback).

These are served **only** when a host opts in via
`LocalServerBuilder::serve_widget(...)` — the K8s/Lambda flavors never mount the
widget routes.

## Keeping it current

Pinned to `@smooai/smooth-operator@1.2.0`. To refresh after a widget release:

```sh
npm pack @smooai/smooth-operator
tar xzf smooai-smooth-operator-*.tgz -C /tmp package/dist/widget/chat-widget.iife.js
cp /tmp/package/dist/widget/chat-widget.iife.js chat-widget.iife.js
```

(A CI step that does this on widget release would keep the two in lockstep.)
