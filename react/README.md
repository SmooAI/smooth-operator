# @smooai/smooth-operator-react

React bindings for the [smooth-operator](../README.md) WebSocket protocol. Two layers, use whichever you need:

- **Headless** — `useConversation` owns the whole connect → session → stream lifecycle and exposes only state + actions. Build any UI you like; this layer has zero styling.
- **Batteries-included** — `<SmoothChat>` and the `parts` components render a complete, accessible chat surface themed entirely by `--smooth-*` **CSS variables**.

It sits on top of `@smooai/smooth-operator` (the protocol client), the same one the [web-component widget](../../smooai-chat-widget) uses — so the two presentation layers are interchangeable over one protocol.

## Install

```bash
pnpm add @smooai/smooth-operator-react react react-dom
```

`react` / `react-dom` are peer dependencies (>=18).

## Quick start — drop-in component

```tsx
import { SmoothChat } from '@smooai/smooth-operator-react';
import '@smooai/smooth-operator-react/styles.css'; // once, anywhere in your app

export function Support() {
    return <SmoothChat url="wss://your-host/ws" agentId="00000000-0000-0000-0000-000000000000" agentName="Support" greeting="How can I help?" />;
}
```

## Headless — bring your own UI

`useConversation` is the whole engine; render its state however you want.

```tsx
import { useConversation } from '@smooai/smooth-operator-react';

function Chat() {
    const { status, messages, send } = useConversation({ url: 'wss://your-host/ws', agentId });

    return (
        <>
            {messages.map((m) => (
                <div key={m.id} data-role={m.role}>
                    {m.text}
                    {m.streaming ? '▋' : null}
                </div>
            ))}
            <button onClick={() => send('hello')} disabled={status !== 'ready'}>
                Send
            </button>
        </>
    );
}
```

`useConversation(options)` returns `{ status, messages, error, sessionId, connect, send, disconnect }`. Each `message` is `{ id, role, text, streaming, citations? }`; assistant `text` grows as `stream_token` events arrive, and `citations` attach from the terminal `eventual_response`.

You can also compose the exported presentational parts (`MessageList`, `MessageBubble`, `Composer`, `Citations`, `ConnectionStatusLabel`) over the hook.

## Theming — CSS variables, not a build coupling

Everything is driven by `--smooth-*` custom properties declared on the `.smooth-chat` root (never `:root`, so nothing leaks into your page). Three ways to retheme, later wins:

**1. Your own CSS / Tailwind** — override the variables anywhere:

```css
.smooth-chat {
    --smooth-color-primary: #4f46e5;
    --smooth-color-bg: #0b1020;
    --smooth-radius: 16px;
}
```

In Tailwind, set them from your design tokens — because they're plain CSS variables, `theme()`, `@layer base`, and `prefers-color-scheme` all just work:

```css
@layer base {
    .smooth-chat {
        --smooth-color-primary: theme(colors.indigo.600);
        --smooth-color-assistant-bubble: theme(colors.slate.100);
    }
}
```

**2. A `theme` prop** (sets the variables inline; wins over stylesheet rules — easiest per-instance override):

```tsx
<SmoothChat url={url} agentId={id} theme={{ primary: '#4f46e5', radius: '16px', surface: '#0b1020' }} />
```

`themeToStyle(theme)` is exported too, for spreading onto your own root element when going headless.

**3. Restyle the `.smooth-chat__*` classes** directly. Treat `styles.css` as a starting point you can fully replace.

### Variables

| Variable                               | `theme` key             | Default                  |
| -------------------------------------- | ----------------------- | ------------------------ |
| `--smooth-color-text`                  | `text`                  | `#0f172a`                |
| `--smooth-color-bg`                    | `background`            | `#ffffff`                |
| `--smooth-color-surface`               | `surface`               | `#f8fafc`                |
| `--smooth-color-primary`               | `primary`               | `#4f46e5`                |
| `--smooth-color-primary-text`          | `primaryText`           | `#ffffff`                |
| `--smooth-color-assistant-bubble`      | `assistantBubble`       | `#eef2ff`                |
| `--smooth-color-assistant-bubble-text` | `assistantBubbleText`   | `#0f172a`                |
| `--smooth-color-user-bubble`           | `userBubble`            | = `primary`              |
| `--smooth-color-user-bubble-text`      | `userBubbleText`        | = `primaryText`          |
| `--smooth-color-border`                | `border`                | `#e2e8f0`                |
| `--smooth-color-muted`                 | `muted`                 | `#64748b`                |
| `--smooth-radius`                      | `radius`                | `12px`                   |
| `--smooth-font`                        | `fontFamily`            | system UI stack          |

Keys match `@smooai/chat-widget`'s `ChatWidgetTheme`, so a brand palette ports between the widget and these components unchanged.

## BYO auth

For deployments where you mint your own identity (Okta/SSO → re-minted JWT), pass an `authToken` — it's appended to the WS URL as `?token=…` (browsers can't set WebSocket headers), which the server reads into the request `Principal` / `AccessContext`:

```tsx
<SmoothChat url="wss://your-host/ws" agentId={id} authToken={shortLivedJwt} />
```

See [Operations / Access Control](../docs/Operations/Access%20Control.md) for the JWT contract and `AUTH_MODE` options.

## Sharing one connection

Wrap a subtree in `SmoothOperatorProvider` and pass a single `SmoothAgentClient` (or `url`) to share one WebSocket across components. `useConversation` accepts a `client` directly too, in which case it doesn't own that client's lifecycle.

## License

MIT.
