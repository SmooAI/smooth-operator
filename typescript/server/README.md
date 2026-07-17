# @smooai/smooth-operator-server

<p>
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-FF6B6C?style=for-the-badge&labelColor=020618" alt="license"></a>
  <a href="https://nodejs.org"><img src="https://img.shields.io/badge/Node-%E2%89%A522-00A6A6?style=for-the-badge&labelColor=020618" alt="Node ≥ 22"></a>
</p>

**Wiring a chat loop is a weekend project. A production agent _server_ is not.**

Sessions that survive a reconnect. A wire protocol your clients can actually speak. Streaming turns you can watch token by token. Tools the model can call — and hard limits on the ones it must never call. Human-in-the-loop when a tool wants to write.

`@smooai/smooth-operator-server` is that server, native to your Node stack. It speaks the [smooth-operator](https://github.com/SmooAI/smooth-operator) wire protocol ([`spec/`](https://github.com/SmooAI/smooth-operator/tree/main/spec)) and runs the published [`@smooai/smooth-operator-core`](https://www.npmjs.com/package/@smooai/smooth-operator-core) engine in-process — one `SmoothAgent` per turn. It's the TS sibling of the [Rust](../../rust/smooth-operator-server), [Go](../../go/server), [Python](../../python/server), and [C#](../../dotnet/server) servers, all speaking the one protocol.

> The published npm SDK in [`../`](../) is the **client** (+ React bindings + chat widget). This is the **server** half.

---

## Spin up a real agent server

```ts
import { serveLocal } from '@smooai/smooth-operator-server';
import { MockLlmProvider } from '@smooai/smooth-operator-core';

// In-memory sessions, auth off, single process. Swap the mock for a gateway client to go live.
const server = await serveLocal({ chatClient: new MockLlmProvider().pushText('hi') });

console.log(`smooth-operator on ${server.url}`); // ws://127.0.0.1:8787/ws
// ... connect a client, drive real streaming turns ...
await server.close(); // graceful drain + stop
```

That's a full agent backend — sessions, streaming turns, tool-calling, citations — on one WebSocket. Or run the binary (defaults to the in-memory local flavor on `127.0.0.1:8787`; set `SMOOAI_GATEWAY_URL` + `SMOOAI_GATEWAY_KEY` for live turns):

```bash
pnpm --filter @smooai/smooth-operator-server build
node dist/main.js
```

With no gateway key, the whole protocol still works — only `send_message` returns a clean `error` until a key is present.

---

## How a turn flows

```
WS connection ──▶ FrameDispatcher ──▶ create/get session ──▶ SessionStore
                       │
                       └── send_message ──▶ TurnRunner ──▶ SmoothAgent.runStream
                                                 │
                       stream_token ◀────────────┤ (per text delta)
                       stream_chunk ◀────────────┤ (per tool call / result)
                       eventual_response ◀───────┘ (terminal, + citations)
```

- **WS transport** (`ws`): per-connection read loop + a single outbound writer — one socket, one writer, never a concurrent `ws.send`.
- **FrameDispatcher** — validates inbound frames and routes them; unknown/invalid frames error *without* dropping the connection.
- **SessionStore** — in-memory sessions + conversation logs behind an async interface; a durable adapter drops in.
- **TurnRunner** — runs the engine streaming and maps `StreamEvent`s onto protocol events, with auto-context citations.
- **Graceful SIGTERM drain** — an in-flight turn finishes before the socket closes; a backplane `detach` always runs after the loop.

---

## Extensible — and safe by construction

An agent is only useful when it can *do* things, and only trustworthy when you can say what it may never do. This server gives you both seams.

**Give it your tools.** Pass them to `serveLocal` and they merge with the built-ins for every turn:

```ts
import { serveLocal, type ServerTool } from '@smooai/smooth-operator-server';

const openTicket: ServerTool = {
    name: 'open_ticket',
    description: 'Open a support ticket for the current customer.',
    parameters: { type: 'object', properties: { subject: { type: 'string' } } },
    supportsAuthRequirement: true, // opt into the authLevel gate below
    async execute(args) {
        return `ticket opened: ${JSON.stringify(args)}`;
    },
};

const server = await serveLocal({ chatClient, tools: [openTicket] });
```

**Or let it gain tools with no redeploy.** The server hosts [SEP extensions](https://github.com/SmooAI/smooth-operator/blob/main/docs/TOOLS.md) — out-of-process tool providers discovered at runtime, their `ui/confirm` prompts bridged into the protocol's confirmation frames for HITL. Gated: an extension contributes tools **only** if you name it in `SMOOTH_EXTENSIONS_ALLOW`. Nothing loads by default.

**Now declare the lines it can't cross.** Register an `AgentConfigResolver`, and every tool — built-in, yours, or from an extension — flows through the same gates:

- **Per-agent allow-list** — an agent's `tool_config.enabledTools` restricts its turn to exactly those tools. Off the list, off the table.
- **The authLevel gate** — a tool marked `supportsAuthRequirement` is *blocked at call time* on a public agent when tagged `admin`, or when tagged `end_user` and the session isn't identity-verified (its OTP bit, or your `SessionAuthenticator` seam). **Fail-closed** — an absent authenticator means "not authenticated".
- **End-user OTP flow** — a refused `end_user` tool can offer a one-time-code identity flow via the `OtpService` seam; the server never generates, holds, or validates a code.

You decide what the agent can touch; the runner enforces it.

---

## Five languages, one protocol

The *same* server — same wire protocol, same conformance corpus — exists in five languages. Run it where your stack already lives.

| Language | Server package | Registry |
| --- | --- | --- |
| **TypeScript** | `@smooai/smooth-operator-server` | in-repo (this package) |
| **Rust** | `smooai-smooth-operator-server` | [crates.io](https://crates.io/crates/smooai-smooth-operator-server) |
| **C# / .NET** | `SmooAI.SmoothOperator.Server` | [in-repo](../../dotnet/server) |
| **Python** | `smooai-smooth-operator-server` | [in-repo](../../python/server) |
| **Go** | `github.com/SmooAI/smooth-operator/go/server` | [in-repo](../../go/server) |

All five run the shared [`spec/conformance/scenarios`](https://github.com/SmooAI/smooth-operator/tree/main/spec/conformance/scenarios) corpus — driven by the engine's deterministic mock, so identical protocol output is a *tested* guarantee (the corpus already caught and fixed real error-handling divergences in the TS and C# servers).

---

## Test

```bash
pnpm --filter @smooai/smooth-operator-server typecheck
pnpm --filter @smooai/smooth-operator-server test
```

27 tests: protocol conformance (round-trips the `spec/conformance/fixtures.json` golden messages), boot, turn round-trip over a real WebSocket (tokens, tool chunks, citations, multi-turn history), graceful drain, the auth verifier seam, and the tool-gating / OTP flow.

## What's done vs. stubbed

| Area | State |
| --- | --- |
| WS transport, single writer, read loop | **Done** |
| FrameDispatcher (ping / create / get / send_message) | **Done** |
| In-memory SessionStore + history replay | **Done** |
| TurnRunner streaming (text + tool chunks + citations) | **Done** |
| Host tool injection (`tools`) + per-agent config + authLevel gate + OTP | **Done** |
| SEP extension hosting (allowlist-gated) | **Done** |
| Auth verifier seam (none / trusted / jwt) | **Done** |
| Graceful SIGTERM/SIGINT drain + detach-after-loop | **Done** |
| `serveLocal()` embeddable entrypoint + binary | **Done** |
| Reranker stage on the citation path | **Stubbed** — the engine supports a reranker; not yet a server knob |
| Cross-pod backplane (Redis/NATS) | **Stubbed** — in-memory `attach`/`detach`; the seam is wired |
| Durable (Postgres/Dynamo) SessionStore | **Stubbed** — implement the `SessionStore` interface |

---

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service. Don't want to run it yourself? **[lom.smoo.ai](https://lom.smoo.ai)** hosts it for you.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
