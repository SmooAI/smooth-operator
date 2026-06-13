<p align="center"><img src="../assets/smooth-logo.svg" alt="Smooth" width="360" /></p>

<p align="center"><strong><code>@smooai/smooth-operator</code></strong> — the Lambda-native TypeScript client for the smooth-operator protocol. Streaming agent turns, HITL resume, fully typed.</p>

<p align="center">
  <a href="../LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License" /></a>
  <img src="https://img.shields.io/badge/tests-16%20passing-success" alt="16 tests passing" />
  <img src="https://img.shields.io/badge/serverless%20%C2%B7%20polyglot%20%C2%B7%20TDD-6f42c1" alt="serverless · polyglot · TDD" />
  <a href="https://lom.smoo.ai"><img src="https://img.shields.io/badge/hosted-lom.smoo.ai-0aa" alt="lom.smoo.ai" /></a>
</p>

---

## What is this?

The **native TypeScript client** for the [smooth-operator](../docs/PROTOCOL.md) WebSocket protocol — and the one the [smooai monorepo dogfoods](https://github.com/SmooAI/smooth-operator). Types are **generated** from the language-neutral JSON Schemas in [`../spec`](../spec) (and committed, so consumers don't need the generator), with an ergonomic layer — discriminated unions + type guards — on top. It's Lambda-native and transport-injectable, so it runs in a browser, on Node, or inside a Lambda handler unchanged.

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
| `@smooai/smooth-operator/react` | React bindings — `useConversation` hook + `<SmoothChat>` (see [the guide](../docs/Guides/React%20Components%20and%20Custom%20UIs.md)) |
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

(Point `url` at your own [`smooth-operator-server`](../rust/README.md), or at the hosted endpoint.)

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

This is one of five native clients generated from the same protocol. Need C# / Microsoft.Extensions.AI? The **`IChatClient` facade** lives in the [.NET client](../dotnet/README.md) (it's a .NET-ecosystem feature). This TypeScript package is the native streaming client.

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

**The proof story:** an LLM-as-judge scored a multi-turn answer **1/5** (the runtime forgot turn 1's context); the failing eval drove a per-session-memory fix; **it now scores 5/5** — a regression a substring test would have missed. See [`docs/EVALS.md`](../docs/EVALS.md).

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

## License

MIT © 2026 Smoo AI
