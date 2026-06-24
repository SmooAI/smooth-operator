<p align="center">
  <a href="https://smoo.ai"><img src="https://raw.githubusercontent.com/SmooAI/smooth-operator/main/.github/banner-typescript.png" alt="@smooai/smooth-operator — the TypeScript client for the smooth-operator protocol." width="100%" /></a>
</p>

<p align="center">
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-F49F0A?style=for-the-badge&labelColor=020618" alt="license"></a>
  <a href="https://lom.smoo.ai"><img src="https://img.shields.io/badge/hosted-lom.smoo.ai-FF6B6C?style=for-the-badge&labelColor=020618" alt="lom.smoo.ai"></a>
  <a href="https://www.npmjs.com/package/@smooai/smooth-operator"><img src="https://img.shields.io/npm/v/@smooai/smooth-operator?style=for-the-badge&labelColor=020618&color=00A6A6" alt="npm"></a>
  <a href="https://nodejs.org"><img src="https://img.shields.io/badge/Node-%E2%89%A522-00A6A6?style=for-the-badge&labelColor=020618" alt="Node ≥ 22"></a>
</p>

<p align="center">
  <b><code>@smooai/smooth-operator</code></b> — the Lambda-native TypeScript client for the <a href="https://github.com/SmooAI/smooth-operator">smooth-operator</a> service.<br/>Streaming agent turns, HITL resume, fully typed. One of <b>five native SDKs</b> over one schema-driven WebSocket protocol.
</p>

---

## What is this?

The **native TypeScript client** for the [smooth-operator](https://github.com/SmooAI/smooth-operator/blob/main/docs/PROTOCOL.md) WebSocket protocol — and the one the [smooai monorepo dogfoods](https://github.com/SmooAI/smooth-operator). It connects to a running smooth-operator **service** (create a session, send a message, stream the agent's events back) — not the agent engine itself. Types are **generated** from the language-neutral JSON Schemas in [`spec/`](https://github.com/SmooAI/smooth-operator/tree/main/spec) (and committed, so consumers don't need the generator), with an ergonomic layer — discriminated unions + type guards — on top. It's Lambda-native and transport-injectable, so it runs in a browser, on Node, or inside a Lambda handler unchanged.

---

## 30-second quickstart

```bash
pnpm add @smooai/smooth-operator
```

Requires Node ≥ 22, ESM only.

### One package, three subpath exports

`@smooai/smooth-operator` is the whole TypeScript SDK — install once, import the
layer you need. `react` / `react-dom` are **optional** peer deps (only the
`./react` subpath needs them), so a client-only or widget-only consumer never
pulls React into their bundle.

| Import | What |
| --- | --- |
| `@smooai/smooth-operator` | the protocol client (`SmoothAgentClient`, streaming turns, HITL) |
| `@smooai/smooth-operator/react` | React bindings — `useConversation` hook + `<SmoothChat>` (see [the guide](https://github.com/SmooAI/smooth-operator/blob/main/docs/Guides/React%20Components%20and%20Custom%20UIs.md)) |
| `@smooai/smooth-operator/react/styles.css` | default stylesheet for the React components |
| `@smooai/smooth-operator/widget` | the embeddable web-component chat widget (`mountChatWidget`, `<smooth-agent-chat>`) |
| `@smooai/smooth-operator/widget/standalone` | the prebuilt IIFE bundle for a no-build `<script>` embed |
| `@smooai/smooth-operator/validate` | the Node-only `ProtocolValidator` (pulls `ajv` + `node:fs`; keep it off the browser path) |

```ts
import { SmoothAgentClient } from '@smooai/smooth-operator';

const client = new SmoothAgentClient({ url: 'ws://127.0.0.1:8787/ws' });
await client.connect();

const session = await client.createConversationSession({ agentId, userName: 'Alice' });
const turn = client.sendMessage({ sessionId: session.sessionId, message: 'How long is your return window?' });

const final = await turn; // EventualResponse — cost, tokens, messageId
console.log(final.data.payload.messageId);
```

(Point `url` at your own [`smooth-operator-server`](https://github.com/SmooAI/smooth-operator/blob/main/rust/README.md), or at the hosted endpoint.)

---

## Watch it stream

`sendMessage` returns a `MessageTurn` that is **both** an async-iterable of events **and** awaitable for the authoritative terminal state. Iterate tokens as they arrive; `await` the same handle for the final response.

```ts
const turn = client.sendMessage({ sessionId: session.sessionId, message: 'Where is my order?' });

for await (const ev of turn) {
  if (ev.type === 'stream_chunk') console.error(`  ↳ node: ${ev.node}`);     // workflow node boundary
  if (ev.type === 'stream_token') process.stdout.write(ev.token ?? '');       // tokens, live
  if (ev.type === 'write_confirmation_required') {
    // HITL: a tool wants to write. Approve, and the resumed stream flows back into THIS turn.
    client.confirmToolAction({ sessionId: session.sessionId, requestId: turn.requestId, approved: true });
  }
}

const final = await turn; // EventualResponse — the authoritative terminal state
```

```mermaid
%%{init: {'theme':'base','themeVariables':{'background':'#020618','primaryColor':'#0b1426','primaryTextColor':'#e6edf6','primaryBorderColor':'#2b3a52','lineColor':'#7c8aa0','actorBkg':'#0b1426','actorBorder':'#2b3a52','actorTextColor':'#e6edf6','signalColor':'#7c8aa0','signalTextColor':'#e6edf6','noteBkgColor':'#f49f0a','noteTextColor':'#1a0f00','noteBorderColor':'#ff6b6c','fontFamily':'ui-sans-serif, system-ui, sans-serif'}}}%%
sequenceDiagram
  participant App
  participant C as SmoothAgentClient
  participant S as Service
  App->>C: sendMessage(...)
  C->>S: { action: send_message }
  S-->>C: immediate_response (202)
  S-->>C: stream_token "Our" "return" "window" …
  S-->>C: stream_chunk { node: response_gen }
  S-->>C: eventual_response (200)
  C-->>App: for-await yields events · await resolves final
```

---

## Transport injection

The client never touches a real socket directly — it talks to an injectable `Transport`. The default uses the global `WebSocket`. On Node, inject the `ws` package; in tests, inject a mock — which is how the conformance suite exercises real client code (correlation, parsing, HITL routing) without a network.

```ts
import WebSocket from 'ws';
new SmoothAgentClient({ url, webSocketFactory: (u) => new WebSocket(u) });
```

## Runtime validation (optional, Node-only)

```ts
import { ProtocolValidator } from '@smooai/smooth-operator';
const v = await ProtocolValidator.load();
v.validateEvent(incomingEvent); // { valid, errors } — ajv-compiled from the spec schemas
```

---

## Polyglot — one spec, five clients

This is one of five native clients generated from the same protocol. Need C# / Microsoft.Extensions.AI? The **`IChatClient` facade** lives in the [.NET client](https://github.com/SmooAI/smooth-operator/tree/main/dotnet/src) (it's a .NET-ecosystem feature). This TypeScript package is the native streaming client.

```mermaid
%%{init: {'theme':'base','themeVariables':{'background':'#020618','primaryColor':'#0b1426','primaryTextColor':'#e6edf6','primaryBorderColor':'#2b3a52','lineColor':'#7c8aa0','secondaryColor':'#0b1426','tertiaryColor':'#0b1426','fontFamily':'ui-sans-serif, system-ui, sans-serif','clusterBkg':'#0b1426','clusterBorder':'#22304a'}}}%%
flowchart LR
  SPEC["spec/ (JSON Schema)"] --> TS["TypeScript<br/>@smooai/smooth-operator"]
  SPEC --> GO["Go"]
  SPEC --> NET[".NET (+ MEAI IChatClient facade)"]
  SPEC --> PY["Python"]
  SPEC --> RS["Rust"]
```

---

## Test-driven by default

> **Nothing here is vibe-coded — it's verified against a real LLM gateway.**

```mermaid
%%{init: {'theme':'base','themeVariables':{'background':'#020618','primaryColor':'#0b1426','primaryTextColor':'#e6edf6','primaryBorderColor':'#2b3a52','lineColor':'#7c8aa0','secondaryColor':'#0b1426','tertiaryColor':'#0b1426','fontFamily':'ui-sans-serif, system-ui, sans-serif','clusterBkg':'#0b1426','clusterBorder':'#22304a'}}}%%
flowchart TD
  J["🎯 LLM-as-judge quality evals (Rust harness)"]
  E["🌐 Live cross-language E2E — this client boots the real server + drives a real claude-haiku-4-5 turn"]
  C["🧪 Conformance fixtures (shared across all 5 clients)"]
  U["⚡ Unit + type-level tests (discrimination, guards, correlation)"]
  J --> E --> C --> U
```

**16 tests** cover the conformance fixtures, the client (with a mock transport so real parsing/correlation/HITL run), and type-level checks. In the **live cross-language E2E**, this client boots a real `smooth-operator-server` subprocess (KB seeded), drives a real `claude-haiku-4-5` turn over WebSocket, and asserts ≥1 streamed event, a knowledge-grounded "17", and per-session memory.

**The proof story:** an LLM-as-judge scored a multi-turn answer **1/5** (the runtime forgot turn 1's context); the failing eval drove a per-session-memory fix; **it now scores 5/5** — a regression a substring test would have missed. See [`docs/EVALS.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/EVALS.md).

Live tests are **gated, never silently skipped**: they run with `SMOOTH_AGENT_E2E=1` + `SMOOAI_GATEWAY_KEY` and skip cleanly otherwise.

```bash
pnpm test          # conformance + client + type-level — no creds
pnpm test:e2e      # live cross-language E2E (needs gateway key)
```

## Scripts

| Script | Purpose |
| --- | --- |
| `pnpm generate` | Regenerate `src/generated/types.ts` from `../spec`. |
| `pnpm build` | `tsc` → `dist/`. |
| `pnpm typecheck` | Type-check `src/` + `test/` without emitting. |
| `pnpm test` | Vitest (conformance + client + type-level). |

The generated types are committed; CI runs `pnpm generate` + `git diff --exit-code` to catch schemas that changed without a regenerate.

## Smoo-powered or bring-your-own

Point the client at the hosted **[lom.smoo.ai](https://lom.smoo.ai)** endpoint, or at your own self-hosted `smooth-operator-server` (AWS Lambda or k8s) — same protocol, same client, same code.

## 🧩 Part of Smoo AI

`@smooai/smooth-operator` is built and open-sourced by **[Smoo AI](https://smoo.ai)** — the AI-powered business platform with AI built into every product. It's the TypeScript member of the **polyglot SDK set** (TypeScript · Python · Go · .NET · Rust) for the [smooth-operator](https://github.com/SmooAI/smooth-operator) service.

- 🌐 **The service** — [smooth-operator](https://github.com/SmooAI/smooth-operator) (protocol, server, the five clients, AWS/k8s deploy)
- 🧰 **More open source from Smoo AI** — [smoo.ai/open-source](https://smoo.ai/open-source)
- ☁️ **Hosted** — [lom.smoo.ai](https://lom.smoo.ai) runs smooth-operator for you, managed and multi-tenant

## 🔗 Links

- 📦 **npm** — [`@smooai/smooth-operator`](https://www.npmjs.com/package/@smooai/smooth-operator)
- 🛰️ **Protocol** — [`docs/PROTOCOL.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/PROTOCOL.md)
- 🧪 **Evals** — [`docs/EVALS.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/EVALS.md)
- 💬 **Issues** — [github.com/SmooAI/smooth-operator/issues](https://github.com/SmooAI/smooth-operator/issues)

## 📄 License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).

---

<p align="center">
  Built by <a href="https://smoo.ai"><strong>Smoo AI</strong></a> — AI built into every product.
</p>
