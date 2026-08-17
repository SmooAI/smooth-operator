<p align="center">
  <a href="https://smoo.ai"><img src=".github/banner.png" alt="smooth-operator — Polyglot AI agent service. One protocol." width="100%" /></a>
</p>

<p align="center">
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-F49F0A?style=for-the-badge&labelColor=020618" alt="license"></a>
  <a href="https://smoo.ai/th"><img src="https://img.shields.io/badge/platform-smoo.ai%2Fth-FF6B6C?style=for-the-badge&labelColor=020618" alt="smoo.ai/th"></a>
</p>

<p align="center">
  <a href="https://github.com/SmooAI/smooth-operator/actions/workflows/typescript.yml"><img src="https://github.com/SmooAI/smooth-operator/actions/workflows/typescript.yml/badge.svg" alt="TypeScript CI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/actions/workflows/go.yml"><img src="https://github.com/SmooAI/smooth-operator/actions/workflows/go.yml/badge.svg" alt="Go CI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/actions/workflows/python.yml"><img src="https://github.com/SmooAI/smooth-operator/actions/workflows/python.yml/badge.svg" alt="Python CI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/actions/workflows/dotnet.yml"><img src="https://github.com/SmooAI/smooth-operator/actions/workflows/dotnet.yml/badge.svg" alt=".NET CI"></a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Kubernetes_·_serverless_·_local-FF6B6C?style=flat-square" alt="Kubernetes · serverless · local">
  <img src="https://img.shields.io/badge/5_languages_·_one_protocol-F49F0A?style=flat-square" alt="5 languages · one protocol">
</p>

<p align="center">
  <a href="#what-is-this"><b>What it is</b></a> &nbsp;·&nbsp; <a href="#quickstart"><b>Quickstart</b></a> &nbsp;·&nbsp; <a href="#deployment-flavors"><b>Deploy flavors</b></a> &nbsp;·&nbsp; <a href="#architecture"><b>Architecture</b></a> &nbsp;·&nbsp; <a href="#-part-of-smoo-ai"><b>Platform</b></a>
</p>

---

> **A chat loop is a weekend project. An agent you'd let near production is not.** Smooth Operator remembers the whole conversation, retrieves only what the person asking is allowed to see, streams its reasoning as it works — and **stops to ask you before it writes anything**. One operator binary that runs the same way on **Kubernetes**, **AWS serverless**, or a **single laptop process**, speaking one protocol to native clients in **five languages**. Built in the open, test-first.
>
> This is the open-source heart of [Smoo AI](https://smoo.ai) — the same operator engine that runs Smooth Operator in your org, MIT-licensed, yours to run. **MIT-licensed. Bring your own model. You approve every write.**

<p align="center">
  <img src=".github/demo-hitl.gif" alt="The operator streams a reply, stops at a knowledge_search tool call for approval, and answers from its knowledge base once approved" width="100%" />
  <br />
  <em>Not a mockup — the <a href="./examples/web-chat">web-chat example</a> against a live server. The turn <b>parks</b> at <code>knowledge_search</code> until a human approves, then answers from the knowledge base.</em>
</p>

---

## What is this?

**smooth-operator** is a **polyglot AI agent service**. The agent orchestration is done by [`smooth-operator-core`](https://github.com/SmooAI/smooth-operator-core) — a 5-language parity engine; the **service** wraps it with conversations, knowledge ingestion + retrieval, a tool catalog, and **one schema-driven WebSocket protocol** that clients in five languages speak natively.

You get hybrid retrieval (dense + sparse + rerank), durable agent checkpoints, human-in-the-loop approvals, and multi-participant conversations (`user` · `ai-agent` · `human-agent`) — behind a stable wire protocol, with **storage, backplane, and auth selected by config**, not by a code fork.

One operator binary, **three deployment flavors** (see [below](#deployment-flavors)):

- **Kubernetes** — the primary self-host target: a long-running service with Postgres + pgvector and a Redis/NATS backplane for multi-replica scale-out.
- **AWS serverless** — API Gateway WebSocket + Lambda + DynamoDB + S3 Vectors, deployed with SST.
- **Local** — a single in-memory process with auth off and zero external services, for laptop dev or to embed in-process.

The same binary picks its flavor from the environment (`SMOOTH_AGENT_STORAGE` · `SMOOTH_AGENT_BACKPLANE` · `AUTH_MODE`). No build flags, no second codebase.

> **Built in the open, test-first.** See [`docs/Planning/Roadmap.md`](docs/Planning/Roadmap.md) for what works today and what's queued.

---

## Quickstart

**Fastest path — Docker.** One command boots the whole stack — Postgres + pgvector, the operator server, and a React chat UI — with token streaming, grounded retrieval with citations, and a human-in-the-loop approval you click yourself. No Rust toolchain required:

```bash
git clone https://github.com/SmooAI/smooth-operator && cd smooth-operator/examples
cp .env.example .env               # set SMOOAI_GATEWAY_KEY — any OpenAI-compatible /v1 gateway works
cd web-chat && docker compose up --build
# → chat UI on http://localhost:8080
```

> First run builds the server image (a few minutes), then it's cached. Prefer a terminal? [`examples/tui-chat`](examples/tui-chat/README.md) drives the same stack from a TUI. Full walkthrough: [`examples/README.md`](examples/README.md).

**From source** — run the reference server natively, fully in-memory: no database, no auth, no AWS. The first compile takes a few minutes; after that it's seconds.

```bash
git clone https://github.com/SmooAI/smooth-operator && cd smooth-operator/rust

# Point at the gateway and seed a distinctive "17-day return window" demo doc.
export SMOOAI_GATEWAY_KEY=sk-…           # your llm.smoo.ai key
export SMOOTH_AGENT_SEED_KB=1            # seeds the demo knowledge docs

cargo run -p smooai-smooth-operator-server
# → smooth-operator-server (local flavor) listening on ws://127.0.0.1:8787/ws (model claude-haiku-4-5)
```

That's it — an agent backend on `ws://127.0.0.1:8787/ws`, with knowledge retrieval, tool-calling, and streaming. With no env set, the binary boots the **local flavor**: in-memory storage, in-memory backplane, loopback bind, admin off. Set `SMOOTH_AGENT_STORAGE=postgres` (or `dynamodb`) and a backplane to graduate the *same* binary to the k8s or serverless flavor.

> No key? The server still boots and answers protocol actions — only `send_message` (which needs the LLM) errors cleanly until `SMOOAI_GATEWAY_KEY` is set.

You can also embed the local flavor **in-process** from Rust — `smooth_operator_server::local::serve_local("127.0.0.1:8787")`, or `LocalServer::builder().seed_kb(true).spawn()` for a handle with a graceful-shutdown switch. See [`deploy/local/README.md`](deploy/local/README.md).

---

## Watch it stream

Connect, start a session, send a turn, and watch tokens stream in — then `await` the authoritative terminal response. Here in TypeScript ([`@smooai/smooth-operator`](typescript/README.md)); the same shape exists in [Go](go/README.md), [.NET](dotnet/README.md), [Python](python/README.md), and [Rust](rust/README.md).

```ts
import { SmoothAgentClient } from '@smooai/smooth-operator';

const client = new SmoothAgentClient({ url: 'ws://127.0.0.1:8787/ws' });
await client.connect();

const session = await client.createConversationSession({ agentId, userName: 'Alice' });

// One turn. Iterate the stream; `await` the same handle for the final state.
const turn = client.sendMessage({ sessionId: session.sessionId, message: 'How long is your return window?' });

for await (const ev of turn) {
  if (ev.type === 'stream_chunk') console.error(`  ↳ node: ${ev.node}`); // knowledge_search, response_gen, …
  if (ev.type === 'stream_token') process.stdout.write(ev.token ?? '');  // "Our return window is 17 days…"
  if (ev.type === 'write_confirmation_required') {
    // HITL: a tool wants to write — approve, and the resumed stream flows back into this same turn.
    client.confirmToolAction({ sessionId: session.sessionId, requestId: turn.requestId, approved: true });
  }
}

const final = await turn; // EventualResponse — cost, tokens, messageId
```

The model autonomously calls `knowledge_search`, retrieves the seeded **17-day** return window, and grounds its answer in it — verified live against `llm.smoo.ai` and across every client.

> Need an embeddable web UI? The TypeScript side ships a [React binding](typescript/src/react) and an [embeddable widget](typescript/src/widget) (a custom element) on top of the same client.

---

## Deployment flavors

One operator binary, one codebase. The `StorageAdapter` + backplane + auth seams are what let the same agent code run on any of three flavors — application code never names a backend. The flavor is selected by config, not by a build.

| | **Kubernetes** (primary self-host) | **AWS serverless** (SST) | **Local** (dev / embed) |
| --- | --- | --- | --- |
| Compute | Long-running pods | API GW WebSocket → Lambda | One in-process server |
| Storage | Postgres + pgvector | DynamoDB + S3 Vectors | In-memory |
| Backplane | Redis / NATS (multi-replica) | API GW connections | In-memory (single process) |
| Auth | `AUTH_MODE=jwt` / `smoo` | `AUTH_MODE=jwt` / `smoo` | `AUTH_MODE=none` (dev only) |
| `SMOOTH_AGENT_STORAGE` | `postgres` | `dynamodb` | `memory` (default) |
| Deploy | `helm install smooth-operator ./deploy/k8s` | `npx sst deploy` in `deploy/sst` | `cargo run -p smooai-smooth-operator-server` |

```bash
# Kubernetes (Helm + ArgoCD) — service + WS ingress, Postgres + pgvector, Redis/NATS backplane
helm install smooth-operator ./deploy/k8s --set image.tag=$(git rev-parse --short HEAD)

# AWS serverless (SST) — API GW WebSocket + Lambda + DynamoDB + S3 Vectors
cd deploy/sst && pnpm install && npx sst deploy --stage prod

# Local — fully in-memory, auth off, no external services
cargo run -p smooai-smooth-operator-server
```

What every flavor **keeps**: hybrid (vector + keyword) retrieval with reranking, a clean Chat · RAG · Agents · Actions decomposition, connector-style ingestion, document-level ACLs over org isolation, and the MIT, batteries-included self-host story. See [`deploy/README.md`](deploy/README.md) and [`docs/DEPLOY.md`](docs/DEPLOY.md) for the full matrix.

---

## Architecture

One protocol in front; a swappable engine and storage behind it. A client never names a language, a backend, or whether the engine is embedded or remote — it only ever sees the protocol.

```mermaid
%%{init: {'theme':'base','themeVariables':{
  'background':'#020618','primaryColor':'#0b1426','primaryTextColor':'#e6edf6','primaryBorderColor':'#2b3a52',
  'lineColor':'#7c8aa0','secondaryColor':'#0b1426','tertiaryColor':'#0b1426','fontFamily':'ui-sans-serif, system-ui, sans-serif',
  'clusterBkg':'#0b1426','clusterBorder':'#22304a'}}}%%
flowchart LR
  CLIENTS["5 native clients<br/>TS · Go · .NET · Python · Rust"]
  CLIENTS -->|"WebSocket protocol"| SVC

  subgraph SVC["smooth-operator · service"]
    PROTO["Protocol layer"] --> RT["KnowledgeChatRuntime"]
  end

  RT -->|"Agent::run"| ENGINE["smooth-operator-core<br/>5-language engine"]
  ENGINE -->|"LlmProvider"| GW[("llm.smoo.ai<br/>or BYO gateway")]
  RT -->|"StorageAdapter"| KB[("Knowledge + conversations<br/>pgvector / DynamoDB + S3 Vectors / in-memory")]

  classDef warm fill:#f49f0a,stroke:#ff6b6c,color:#1a0f00;
  classDef teal fill:#00a6a6,stroke:#00c2c2,color:#011;
  class ENGINE warm
  class GW,KB teal
```

### An agent turn, end to end

```mermaid
%%{init: {'theme':'base','themeVariables':{
  'background':'#020618','primaryColor':'#0b1426','primaryTextColor':'#e6edf6','primaryBorderColor':'#2b3a52',
  'lineColor':'#7c8aa0','actorBkg':'#0b1426','actorBorder':'#2b3a52','actorTextColor':'#e6edf6',
  'signalColor':'#7c8aa0','signalTextColor':'#e6edf6','noteBkgColor':'#f49f0a','noteTextColor':'#1a0f00','noteBorderColor':'#ff6b6c',
  'fontFamily':'ui-sans-serif, system-ui, sans-serif'}}}%%
sequenceDiagram
  participant C as Client
  participant S as Service
  participant A as Agent
  participant K as Knowledge / Tools
  participant L as LLM gateway

  C->>S: send_message { sessionId, message }
  S->>A: run turn (replay prior messages)
  S-->>C: immediate_response (202, ack)
  A->>K: knowledge_search("return window")
  K-->>A: top-K snippets (the 17-day fact)
  A->>L: chat completion (grounded prompt)
  L-->>A: token deltas …
  A-->>S: TokenDelta / PhaseStart / ToolCallComplete
  S-->>C: stream_token "Our" "return" "window" …
  S-->>C: stream_chunk { node: response_gen }
  A-->>S: Completed { cost, tokens }
  S-->>C: eventual_response (200, final)
```

### Protocol lifecycle (incl. HITL)

```mermaid
%%{init: {'theme':'base','themeVariables':{
  'background':'#020618','primaryColor':'#0b1426','primaryTextColor':'#e6edf6','primaryBorderColor':'#2b3a52',
  'lineColor':'#7c8aa0','secondaryColor':'#0b1426','tertiaryColor':'#0b1426','fontFamily':'ui-sans-serif, system-ui, sans-serif'}}}%%
stateDiagram-v2
  [*] --> Connected: connect
  Connected --> SessionOpen: create_session
  SessionOpen --> Streaming: send_message
  Streaming --> Streaming: stream_token · chunk
  Streaming --> AwaitingApproval: confirm_required
  AwaitingApproval --> Streaming: approve
  Streaming --> AwaitingOtp: otp_required
  AwaitingOtp --> Streaming: verify_otp
  Streaming --> SessionOpen: eventual_response
  SessionOpen --> [*]: disconnect
```

Full action/event tables, the `AgentEvent` mapping, and connection-state keys are in [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

---

## Extensible — and safe by construction

An agent is only useful when it can *do* things, and only trustworthy when you can say what it may never do. The server gives you both seams — and they're the emotional core of the whole design.

**Give it your tools.** Install a tool provider (the `ToolProvider` seam in Rust, `tools` in the TS/Python/Go/.NET servers) and the runner merges your tools with the built-ins for every turn — scoped to the turn's org and the caller's entitlements, so a per-org CRM lookup or a ticketing action drops in without the shared core ever learning your schema.

**Let it gain tools with no redeploy.** The server hosts **SEP extensions** — out-of-process tool providers discovered at runtime and attached to the turn, their `ui/confirm` prompts bridged straight into the protocol's confirmation frames for human-in-the-loop. It's gated: an extension contributes tools **only** if you name it in `SMOOTH_EXTENSIONS_ALLOW`. Nothing loads by default.

**Then declare the lines it can't cross.** Every tool — built-in, host-provided, or from an extension — flows through the same gates, so the guardrails hold no matter where a tool came from:

- **Per-agent allow-list** — an agent's `tool_config.enabledTools` restricts its turn to exactly those tools. Off the list, off the table.
- **The auth-level `ToolHook`** — a tool tagged `admin` or `end_user` is *blocked at call time* on a public agent unless the caller is verified (the session's OTP bit, or your `SessionAuthenticator` seam). The hook runs before the tool does, and **fails closed**.
- **Document-level ACLs** — both retrieval paths read through the storage adapter's access-scoped view, so a document the requester isn't entitled to is dropped before it can reach the model or land in a citation.

That's what "point it at prod" costs here: not a leap of faith, a declaration. You decide what the agent can touch; the runner enforces it. See [`docs/TOOLS.md`](docs/TOOLS.md) and [`docs/ACCESS-CONTROL.md`](docs/ACCESS-CONTROL.md).

---

## Five languages, one protocol

The same server, the same wire protocol, in the language your stack already speaks. Every client connects to every server, unmodified — a *tested* guarantee, since all five servers run the shared [`spec/conformance/scenarios`](spec/conformance/scenarios) corpus.

| Language | Client package | Server package | Registry |
| --- | --- | --- | --- |
| **TypeScript** | `@smooai/smooth-operator` | `@smooai/smooth-operator-server` | [npm](https://www.npmjs.com/package/@smooai/smooth-operator) |
| **Python** | `smooai-smooth-operator` | `smooai-smooth-operator-server` | [PyPI](https://pypi.org/project/smooai-smooth-operator/) |
| **Rust** | `smooai-smooth-operator` | `smooai-smooth-operator-server` | [crates.io](https://crates.io/crates/smooai-smooth-operator-server) |
| **.NET** | `SmooAI.SmoothOperator` | `SmooAI.SmoothOperator.Server` | [NuGet](https://www.nuget.org/packages/SmooAI.SmoothOperator.Server) |
| **Go** | `…/smooth-operator/go` | `…/smooth-operator/go/server` | [pkg.go.dev](https://pkg.go.dev/github.com/SmooAI/smooth-operator/go) |

Every client ships to its registry today except .NET (in-repo for now). Servers: **Rust on crates.io, Python on PyPI, and three .NET packages on NuGet** (`Server`, `Server.AspNetCore`, `Server.Postgres`); the TypeScript and Go servers live in-repo (`typescript/server`, `go/server`). The TS side also ships a **React binding** and an **embeddable web-component widget** as subpath exports of the same npm package.

One protocol, defined once in [`spec/`](spec) (JSON Schema). Everything else is generated or hand-written to match it — here's the honest status of each surface:

| Surface | Status |
| --- | --- |
| **Engine** ([`smooth-operator-core`](https://github.com/SmooAI/smooth-operator-core)) | **5-language parity engine** — Rust · C# · Python · TypeScript · Go, each published (crates.io / NuGet / PyPI / npm / Go module). Rust is the reference; the others mirror its surface. Every engine capability is in all five today except **one**, stated plainly in [PARITY-STATUS.md](PARITY-STATUS.md): the extension **sandbox / integrity hardening** is Rust-first. The durable-execution **backend** (Temporal) now ships as an optional per-language package in **all five**, with the server-side selection seam in all five servers — leaving only the one shared ADR-030 streaming/cost follow-up that applies equally to every language, Rust included. |
| **Protocol clients** | **All five languages** — TypeScript (`@smooai/smooth-operator`), Go, .NET (with a `Microsoft.Extensions.AI` `IChatClient` facade), Python, Rust. The TS side also ships a **React binding** and an **embeddable widget**. |
| **Servers** | **All five languages** — Rust · C# · Python · TypeScript · Go, each consuming its own language's engine so a host can run the full service in its native stack. All five carry the transport core: frame dispatch · per-turn engine · sessions · auth · graceful drain, plus a Postgres store for conversations/messages/participants/sessions. Depth past that is uneven and worth knowing before you pick one: **Rust** is the only server with pluggable storage backends (Postgres *and* DynamoDB) and the only one with a **cross-pod backplane** (Redis / NATS) — the rest run an in-memory backplane, so they are single-replica. **C#** adds persistent checkpoint, knowledge-base and ACL-knowledge stores, and carries the deepest ingestion/ACL surface after Rust, but ships **no backplane at all** today. **Go, TypeScript and Python** now also carry the durable **knowledge + ACL-knowledge** Postgres stores (the persistent **checkpoint** store is still Rust/.NET-only, and the TS/Python knowledge stores aren't yet wired into the live dispatcher). **All five servers now emit `gen_ai.*` OpenTelemetry spans** (chat + tool, redacted tool args, env-gated OTLP). **All five run the shared scenario conformance corpus** — driven by the engine's deterministic mock, so they must produce identical protocol output. The corpus already caught and fixed real error-handling divergences in the TS and C# servers. See [PARITY-STATUS.md](PARITY-STATUS.md) for the verified breakdown. |

---

## Test-driven by default

> **Nothing here is vibe-coded — it's verified against a real LLM gateway.** Substring tests prove a reply *contains* the right number; an LLM-as-judge proves the agent *reasoned* its way there and didn't hallucinate. We run both.

```mermaid
%%{init: {'theme':'base','themeVariables':{
  'background':'#020618','primaryColor':'#0b1426','primaryTextColor':'#e6edf6','primaryBorderColor':'#2b3a52',
  'lineColor':'#7c8aa0','secondaryColor':'#0b1426','tertiaryColor':'#0b1426','fontFamily':'ui-sans-serif, system-ui, sans-serif'}}}%%
flowchart TD
  U["Unit tests<br/>chunker · SSRF guard · can_access"] --> C
  C["Testcontainers conformance<br/>pgvector + DynamoDB-Local"] --> E
  E["Live cross-language E2E<br/>all 5 clients, real WebSocket turns"] --> J
  J["LLM-as-judge quality evals<br/>real gateway, rubric-scored 1–5"]

  classDef warm fill:#f49f0a,stroke:#ff6b6c,color:#1a0f00;
  classDef teal fill:#00a6a6,stroke:#00c2c2,color:#011;
  class U teal
  class J warm
```

All five native servers run a **shared scenario conformance corpus** ([`spec/conformance/scenarios`](spec/conformance/scenarios)) — language-neutral protocol flows driven by the engine's deterministic mock, so every server must produce identical output. That's the polyglot parity oracle, on top of each server's own protocol/ingestion/ACL/rerank/embedder suites and the engine's offline suite ([smooth-operator-core](https://github.com/SmooAI/smooth-operator-core), hundreds of tests on a deterministic `MockLlmClient`). The five protocol clients are exercised against a real WebSocket in a cross-language E2E harness.

### The proof story

The headline isn't a count — it's a **real defect a substring test would have missed**. On the first live run, our LLM-as-judge scored a multi-turn answer **1/5**: the runtime built a fresh agent per turn, so turn 2 had no memory of turn 1's delivery date and couldn't compute the last return day. A `contains("the 22nd")` assertion would have stayed green on a hallucinated guess. The judge caught it; the fix wired per-session memory; **it now scores 5/5**.

That's the whole bet: quality regressions that only a grader can see, caught in CI. Details — the five scenarios, the rubric, the same-model-judge knob — in [`docs/EVALS.md`](docs/EVALS.md).

### Gated, never silently skipped

Live tests need a gateway key. They are **gated, not deleted**: with `SMOOTH_AGENT_E2E=1` + `SMOOAI_GATEWAY_KEY` they run (and print every per-scenario score under `--nocapture`); without them they print an explicit **skip** and return — so credential-free `cargo test` and CI stay green, and the nightly job runs the full live suite. The gateway key is read from the environment and **never printed**.

```bash
# Unit + conformance — no creds, runs everywhere
cd rust && cargo test

# + live LLM-as-judge evals
export SMOOAI_GATEWAY_KEY=sk-… SMOOTH_AGENT_E2E=1
cargo test -p smooai-smooth-operator-evals --test llm_judge -- --nocapture --test-threads=1
```

---

## Smoo-powered or bring-your-own

A recurring principle across the whole stack: **same code, two postures.**

| Capability      | Smoo-powered (hosted)             | Bring-your-own (self-host)               |
| --------------- | --------------------------------- | ---------------------------------------- |
| LLM gateway     | `llm.smoo.ai`                     | any OpenAI-compatible endpoint           |
| Embeddings      | gateway (`text-embedding-3-small`) | `DeterministicEmbedder` or your provider |
| Web search      | Smoo provider                     | Brave / Bing / Tavily via `WebSearchProvider` |
| Identity / RBAC | Smoo identity (`AUTH_MODE=smoo`)  | `AUTH_MODE=jwt` (BYO JWT/OIDC)           |
| Connectors      | managed GitHub/Slack apps         | your tokens, same `Connector` trait      |

Self-host brings their own; hosted wires Smoo's apps. The seams are identical — see [`docs/INGESTION.md`](docs/INGESTION.md), [`docs/TOOLS.md`](docs/TOOLS.md), and [`docs/STORAGE.md`](docs/STORAGE.md).

---

## The two-repo split

| Repo | What it is |
| ---- | ---------- |
| [`smooth-operator-core`](https://github.com/SmooAI/smooth-operator-core) | The **agent engine** — `Agent`, `Workflow`, `Tool`, `CheckpointStore`, `LlmProvider`, `Memory`, `KnowledgeBase`. A **5-language parity engine** (Rust · C# · Python · TypeScript · Go), each published. |
| **`smooth-operator`** (this repo) | The **service** — conversations, knowledge ingestion + retrieval, the tool catalog, the WebSocket protocol, the five clients, the management console, and the Kubernetes / AWS / local deploy flavors. |

## Repository layout

```
smooth-operator/
├── spec/         # The language-neutral wire protocol (JSON Schema) — source of truth for all clients
├── rust/         # Reference server + service crate (smooai-smooth-operator) + adapters, lambda, evals, ingestion
├── typescript/   # @smooai/smooth-operator — client + React binding + embeddable widget
├── go/           # github.com/SmooAI/smooth-operator/go — protocol.Client
├── dotnet/       # SmooAI.SmoothOperator — client (+ Microsoft.Extensions.AI facade) and the C# server
├── python/       # smooth-operator (import smooth_operator) — async client
├── console/      # Next.js management console for the auth-gated /admin/* API
├── examples/     # Runnable reference apps — web-chat (Vite+React) & tui-chat (terminal); each a `docker compose up` stack with Postgres
├── adapters/     # Pointer only — the storage adapter crates live in rust/adapters/ (postgres + dynamodb)
├── deploy/
│   ├── k8s/      # Kubernetes (Helm + ArgoCD) — Postgres + pgvector + Redis/NATS backplane
│   ├── sst/      # AWS serverless (API GW WebSocket + Lambda + DynamoDB + S3 Vectors)
│   └── local/    # Local / embed-in-process — in-memory, auth off, no external services
└── docs/         # Architecture, protocol, storage, evals, ingestion, access-control, observability, deploy, roadmap
```

## Run it hosted

Don't want to operate it yourself? smooth-operator powers the **[Smoo AI platform](https://smoo.ai)** in production today, and a standalone managed offering is on the [roadmap](docs/Planning/Roadmap.md).

## Documentation

| Doc | What |
| --- | --- |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | System design, the agent pipeline, how it consumes the engine |
| [`docs/PROTOCOL.md`](docs/PROTOCOL.md) | The schema-driven WebSocket protocol |
| [`docs/STORAGE.md`](docs/STORAGE.md) | The `StorageAdapter` trait; Postgres and DynamoDB/S3 Vectors designs |
| [`docs/EVALS.md`](docs/EVALS.md) | The LLM-as-judge quality harness (the 1/5 → 5/5 story) |
| [`docs/INGESTION.md`](docs/INGESTION.md) | Connectors, chunking, the embedder seam |
| [`docs/TOOLS.md`](docs/TOOLS.md) | The built-in tool catalog + authoring your own |
| [`docs/ACCESS-CONTROL.md`](docs/ACCESS-CONTROL.md) | Document-level ACLs over org isolation |
| [`docs/ADMIN-API.md`](docs/ADMIN-API.md) | The auth-gated `/admin/*` API the console consumes |
| [`examples/web-chat/`](examples/web-chat/README.md) | A runnable Vite + React chat client — `docker compose up` (Postgres + operator + UI): streaming, inline tool viz, HITL approvals, sidebar |
| [`examples/tui-chat/`](examples/tui-chat/README.md) | A dependency-free terminal chat client — `docker compose run tui` (same stack): streaming, tool chips, HITL approvals, `/list`/`/resume` |
| [`docs/OBSERVABILITY.md`](docs/OBSERVABILITY.md) | OpenTelemetry `gen_ai.*` tracing |
| [`docs/DEPLOY.md`](docs/DEPLOY.md) | The three deploy flavors + the shared `SmooAI/deploy` package |
| [`docs/Planning/Roadmap.md`](docs/Planning/Roadmap.md) | Phased build plan + current status |

## 🧩 Part of Smoo AI

smooth-operator is built and open-sourced by **[Smoo AI](https://smoo.ai)** — the AI-powered business platform with AI built into every product: CRM, customer support, campaigns, field service, observability, and developer tools.

- 🚀 **smooth-operator on the platform** — [smoo.ai/th](https://smoo.ai/th)
- 🧰 **More open source from Smoo AI** — [smoo.ai/open-source](https://smoo.ai/open-source)
- 🧩 **Sibling packages** — [smooth-operator-core](https://github.com/SmooAI/smooth-operator-core) (the 5-language engine this wraps), [@smooai/deploy](https://github.com/SmooAI/deploy), [smooth](https://github.com/SmooAI/smooth) (the `th` CLI)
- ☁️ **Hosted** — smooth-operator runs the [Smoo AI platform](https://smoo.ai) in production; a standalone managed offering is on the [roadmap](docs/Planning/Roadmap.md)

## 🤝 Contributing

Built in the open, test-first. Issues and PRs welcome — see the [docs vault](docs/Home.md) for architecture, protocol, and the eval harness, and [`docs/Planning/Roadmap.md`](docs/Planning/Roadmap.md) for what's queued.

## 📄 License

MIT © 2026 Smoo AI. See [LICENSE](LICENSE).

---

<p align="center">
  Built by <a href="https://smoo.ai"><strong>Smoo AI</strong></a> — AI built into every product.
</p>
