# React Components and Custom UIs

smooth-operator's frontend story is **layered and modular** — you can adopt as
much or as little UI as you want, because every layer sits on the same
schema-driven WebSocket protocol. Pick the layer that matches how much control
you need:

```mermaid
flowchart TD
  P["WebSocket protocol<br/>(spec/ — language-neutral)"]
  C["@smooai/smooth-operator<br/>headless TS client"]
  W["@smooai/chat-widget<br/>web component (any framework / no build)"]
  H["@smooai/smooth-operator-react · useConversation<br/>headless hook (your own UI)"]
  S["@smooai/smooth-operator-react · &lt;SmoothChat&gt;<br/>styled component (CSS-variable themed)"]
  P --> C --> W
  C --> H --> S
```

| You want…                                              | Use                                        |
| ------------------------------------------------------ | ------------------------------------------ |
| Chat on a page, any framework, maybe no build step     | the chat widget (`@smooai/chat-widget`, web component)         |
| A React app, your own components, full design control  | `useConversation` (headless hook)           |
| A React app, batteries-included chat to theme          | `<SmoothChat>` (CSS-variable themed)        |
| Another language, or a totally custom surface          | the [[Using the Polyglot Clients\|clients]] directly |

The package is `@smooai/smooth-operator-react` (source in `react/`). `react` /
`react-dom` are peer deps; the protocol client is a normal dependency it
re-exports for convenience.

## Headless first — `useConversation`

The hook owns the whole lifecycle (connect → create session → stream tokens →
finalize with citations) and returns **only state + actions** — no markup, no
styling. This is the modular core: build any UI on top.

```tsx
import { useConversation } from '@smooai/smooth-operator-react';

function Chat({ url, agentId }: { url: string; agentId: string }) {
    const { status, messages, send } = useConversation({ url, agentId });
    // messages: { id, role, text, streaming, citations? }[]
    // text grows as stream_token events arrive; citations attach on the terminal event.
    return /* render however you like */;
}
```

Returns `{ status, messages, error, sessionId, connect, send, disconnect }`.
Pair it with the exported parts (`MessageList`, `MessageBubble`, `Composer`,
`Citations`, `ConnectionStatusLabel`) or render the state yourself.

## Batteries-included — `<SmoothChat>`

```tsx
import { SmoothChat } from '@smooai/smooth-operator-react';
import '@smooai/smooth-operator-react/styles.css'; // once, anywhere

<SmoothChat url="wss://your-host/ws" agentId={agentId} agentName="Support" greeting="How can I help?" />;
```

## Theming with CSS variables (not a build coupling)

The deliberate choice here: **components are themed by `--smooth-*` CSS custom
properties**, *not* by a shared Tailwind config. Shipping compiled components
that you "theme through tailwind config" doesn't actually work — a consumer's
`tailwind.config` can't reach class names baked into `node_modules`. CSS
variables travel cleanly regardless of whether you even use Tailwind, and they
compose with `theme()`, `@layer`, design tokens, and `prefers-color-scheme`.

Defaults are declared on the `.smooth-chat` root (never `:root`, so nothing
leaks into your page). Override them three ways, later wins:

1. **Your CSS / Tailwind** — `.smooth-chat { --smooth-color-primary: theme(colors.indigo.600); }`
2. **A `theme` prop** — `<SmoothChat theme={{ primary: '#4f46e5', radius: '16px' }} />` (sets the vars inline; wins over stylesheet rules). `themeToStyle(theme)` is exported for headless use.
3. **Restyle `.smooth-chat__*`** classes outright.

The variable ↔ `theme`-key table is in the package
[README](../../react/README.md). Keys match the widget's `ChatWidgetTheme`, so a
brand palette ports between the web-component widget and the React components
unchanged.

## Auth

Pass `authToken` to either `<SmoothChat>` or `useConversation` for BYO-auth — it
rides the WS URL as `?token=…` into the server's `Principal` / `AccessContext`.
See [[Integrating into an Existing App]] and [[Access Control]].

## Related

- [[Using the Polyglot Clients]] — driving the protocol from TS/Go/.NET/Python; includes the `@smooai/chat-widget` web component.
- [[Integrating into an Existing App]] — auth modes for embedding.
- [[Protocol Reference]] — the message/frame contract underneath all of this.
