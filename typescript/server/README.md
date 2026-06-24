# @smooai/smooth-operator-server

A native **TypeScript server** for the smooth-operator wire protocol — the TS
sibling of the Rust (`rust/smooth-operator-server`) and C# (`dotnet/server`)
servers. It speaks the same protocol (`spec/`) and runs the published
[`@smooai/smooth-operator-core`](https://www.npmjs.com/package/@smooai/smooth-operator-core)
engine in-process, one `SmoothAgent` per turn.

The published TS SDK in `../` (the **client** + React bindings + chat widget) is
untouched; this is the **server** half.

## What it does

```
WS connection ──▶ FrameDispatcher ──▶ create/get session ──▶ SessionStore
                       │
                       └── send_message ──▶ TurnRunner ──▶ SmoothAgent.runStream
                                                 │
                       stream_token ◀────────────┤ (per text delta)
                       stream_chunk ◀────────────┤ (per tool call / result)
                       eventual_response ◀───────┘ (terminal, + citations)
```

- **WS transport** (`ws`): per-connection read loop + a single outbound writer
  (one socket = one writer; `ws.send` is never called concurrently).
- **FrameDispatcher** — validates inbound frames, routes
  `create_conversation_session` → `SessionStore`, `send_message` → `TurnRunner`,
  `ping` → pong. Unknown/invalid frames error without dropping the connection.
- **SessionStore** — in-memory sessions + conversation message logs (async
  interface; a durable adapter drops in).
- **TurnRunner** — runs the engine streaming, maps `StreamEvent`s (`text` →
  `stream_token`, `tool_call`/`tool_result` → `stream_chunk`) onto protocol
  events, and returns the final reply + auto-context citations.
- **Auth verifier seam** — `none` (default) / `trusted` (base64url JSON from a
  trusted proxy) / `jwt` (HS256). Fail-closed: anything missing/malformed/expired
  → anonymous.
- **Graceful SIGTERM drain** — a shared `AbortController`; each connection's read
  loop races "cancel fired" vs "next inbound frame" with the turn dispatch awaited
  inside the frame branch, so an in-flight turn finishes; a backplane `detach`
  always runs after the loop exits.
- **Backplane seam** — in-memory stub today (`attach`/`detach`); the wiring for a
  Redis/NATS cross-pod backplane is in place.

## Use it

```ts
import { serveLocal } from '@smooai/smooth-operator-server';
import { MockLlmProvider } from '@smooai/smooth-operator-core';

const server = await serveLocal({ chatClient: new MockLlmProvider().pushText('hi') });
console.log(`smooth-operator on ${server.url}`);
// ... connect a client, run turns ...
await server.close(); // graceful drain + stop
```

Or run the binary (defaults to the in-memory local flavor on `127.0.0.1:8787`;
set `SMOOAI_GATEWAY_URL` + `SMOOAI_GATEWAY_KEY` for live turns):

```bash
pnpm --filter @smooai/smooth-operator-server build
node dist/main.js
```

## Test

```bash
pnpm --filter @smooai/smooth-operator-server typecheck
pnpm --filter @smooai/smooth-operator-server test
```

27 tests: protocol conformance (round-trips the `spec/conformance/fixtures.json`
golden messages), boot, turn round-trip over a real WebSocket (tokens, tool
chunks, citations, multi-turn history), graceful drain, and the auth verifier seam.

## MVP-stubbed vs done

| Area | State |
| --- | --- |
| WS transport, single writer, read loop | **Done** |
| FrameDispatcher (ping / create / get / send_message) | **Done** |
| In-memory SessionStore + history replay | **Done** |
| TurnRunner streaming (text + tool chunks + citations) | **Done** |
| Auth verifier seam (none / trusted / jwt) | **Done** |
| Graceful SIGTERM/SIGINT drain + detach-after-loop | **Done** |
| `serveLocal()` embeddable entrypoint + binary | **Done** |
| HITL tool-confirm / OTP / write-confirmation events | **Stubbed** — the turn seam exists; the confirm round-trip is follow-on |
| Reranker stage on the citation path | **Stubbed** — the engine supports a reranker; not yet surfaced as a server knob |
| ACL-filtered knowledge (`AccessKnowledge.forAccess`) | **Seam present** — the MVP provider is unscoped; a group-filtered view drops in |
| Cross-pod backplane (Redis/NATS) | **Stubbed** — in-memory `attach`/`detach`; the seam is wired |
| Durable (Postgres/Dynamo) SessionStore | **Stubbed** — implement the `SessionStore` interface |
