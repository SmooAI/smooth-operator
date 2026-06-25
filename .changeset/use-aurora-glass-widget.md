---
"@smooai/smooth-operator": patch
---

local flavor: serve the canonical `@smooai/chat-widget` (Aurora Glass) bundle

The local-flavor server now vendors and serves the published **`@smooai/chat-widget`**
(Aurora Glass) standalone bundle instead of a parallel copy of the widget. One canonical
public widget, consumed — not two. Same `<smooth-agent-chat>` element + `endpoint`/`agent-id`
attributes, so it's a drop-in for the host page.
