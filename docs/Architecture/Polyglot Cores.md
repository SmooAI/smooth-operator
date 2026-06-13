# Polyglot Cores

> **Status:** active. The **C# core is complete** (all phases shipped, published to NuGet).
> The Rust engine `smooai-smooth-operator-core` is the **reference implementation**.

## The idea

smooth-operator has, until now, been **one Rust engine + polyglot protocol clients**:
every other language talked to a Rust server over the WebSocket protocol (a network
hop). A **polyglot core** goes further — it reimplements the *engine itself* natively
in each target language, so the agent runs **in-process** in that language's runtime,
with native tools, native debugging, and no separate service to operate.

Each language core learns from that ecosystem's best agent framework rather than
transliterating Rust:

| Language | Learns from | LLM/model abstraction |
| --- | --- | --- |
| **Rust** (reference) | — | `LlmProvider` trait |
| **C#** (first sibling) | **Microsoft Agent Framework (MAF)** + `Microsoft.Extensions.AI` | `IChatClient` |
| *(future)* Python | LangGraph / Agno | (TBD) |
| *(future)* TypeScript | — | (TBD) |

This is deliberately **not** the FFI-bindings approach (one Rust core exposed to every
language via uniffi/napi/PyO3). Async + streaming across an FFI boundary is painful,
and FFI bindings don't *feel* native. Polyglot cores cost more to maintain, but each
one is a first-class library in its ecosystem. What keeps them honest is a **shared
behavioral contract**, not shared code.

## The shared contract — parity is enforced, not assumed

A sibling core is "correct" when it passes the same three layers the Rust reference
passes. It does **not** have to mirror Rust's type shapes — only its behavior.

1. **Protocol conformance** — every instance in [`spec/conformance/fixtures.json`](../../spec/conformance/fixtures.json)
   round-trips its declared JSON Schema. (Relevant when the core is wrapped in the
   server/protocol layer; the engine itself is protocol-agnostic.)
2. **Behavioral parity unit tests** — the engine's defining behaviors, ported as the
   language's idiomatic tests. The canonical Phase-0 assertion, from the Rust core:
   *a text response with no tool calls ends the loop after exactly one LLM call, and
   the user's message reached the model.* Each phase below adds its parity tests.
3. **Eval scenarios** — the five [`rust/evals`](../../rust/evals) scenarios
   (`grounded_answer`, `honest_no_knowledge`, `tool_use_supported_answer`,
   `multi_turn_coherence`, `tone_helpfulness`), judged by an LLM against the same
   rubric, mean score ≥ 4.0. A sibling core runs the *same* scenarios against a live
   model and must clear the same bar.

If a behavior isn't covered by one of these three, it isn't part of the contract yet —
add the test to the reference first, then port it.

## Engine concept inventory (Rust → C#)

What every core implements, and how the C# core expresses each on MEAI/MAF idioms.
The C# core **reuses MEAI primitives** wherever MAF already has the right abstraction,
and only invents types for the agentic machinery MEAI doesn't provide (the loop,
checkpoints, cast, HITL, cost, memory/knowledge injection).

| Rust engine | C# core | Notes |
| --- | --- | --- |
| `LlmProvider` trait | `Microsoft.Extensions.AI.IChatClient` | any MEAI provider plugs in (Azure OpenAI, OpenAI, Ollama, the smooth gateway) |
| `Message` / `Role` | `ChatMessage` / `ChatRole` | reuse MEAI |
| `Tool` trait / `ToolSchema` | `AIFunction` / `AITool` (`AIFunctionFactory.Create`) | a normal C# method becomes a tool |
| `ToolCall` / `ToolResult` | `FunctionCallContent` / `FunctionResultContent` | reuse MEAI content types |
| `StreamEvent` | `ChatResponseUpdate` / `AgentRunResponseUpdate` | MAF streaming shape |
| `Agent` / `agent.run()` / `run_with_channel()` | `SmoothAgent` / `RunAsync` / `RunStreamingAsync` | MAF naming; **we own the loop** |
| `AgentEvent` enum | `AgentRunResponseUpdate` + typed run events | |
| `MockLlmClient` | `MockChatClient : IChatClient` | scripted test double |
| `Memory` / `KnowledgeBase` | `IAgentMemory` / `IKnowledgeBase` | injected as context pre-turn |
| `CheckpointStore` | `ICheckpointStore` | thread state save/resume |
| `Cast` / `OperatorRole` / `DispatchSubagentTool` | `Cast` / `OperatorRole` / dispatch tool | maps to MAF handoff/group-chat patterns |
| HITL (`HumanRequest`/`HumanResponse`) | `IHumanGate` pause/resume | tool write-confirmation + input |
| `CostTracker` / `CostBudget` | `CostTracker` / `CostBudget` | usage accounting + budget enforcement |

## Phased roadmap (C#)

Mirrors how the Rust core was bootstrapped (harness first, then layer up). Each phase
ships green parity tests before the next starts.

- **Phase 0 — harness + agentic loop** *(shipped)*: `IChatClient`-driven loop,
  `AIFunction` tools, `MockChatClient`, `RunAsync`/`RunStreamingAsync`, max-iteration
  guard, usage accumulation. Parity test: text-turn ends after one call; tool-turn
  executes the function and feeds the result back.
- **Phase 1 — conversation + compaction** *(shipped)*: `SmoothAgentThread` carries
  history across turns; `MaxContextTokens` budget with a `SlidingWindow` compaction
  strategy (preserves system + latest user). Parity tests: multi-turn continuity;
  compaction trims old messages under budget.
- **Phase 2 — memory + knowledge** *(shipped)*: pluggable `IKnowledgeBase` /
  `IAgentMemory` (with deterministic in-memory lexical impls); the agent retrieves
  the top-K hits for the user's message and injects them as grounding context before
  answering (RAG). Parity tests: ranked retrieval, knowledge + memory injection,
  no-hit injects nothing.
- **Phase 3 — checkpointing + resume** *(shipped)*: `ICheckpointStore` +
  `CheckpointStrategy` (Never/AfterEachIteration/AfterToolCall); the agent snapshots the
  durable conversation during a run, and `ResumeThreadAsync` rebuilds a thread from its
  latest checkpoint (resume-or-new). `InMemoryCheckpointStore` ships; file/SQLite/Postgres
  next. Parity tests: store ops, checkpoint-after-tool-call, resume-across-restart.
- **Phase 4 — HITL** *(shipped)*: `IHumanGate` (+ `DelegateHumanGate`) the agent consults
  before any tool call flagged by `AgentOptions.RequiresApproval`. The gate's async call is
  the pause point (a UI awaits a real person); a denial is fed back to the model and the
  tool never runs. Parity tests: deny blocks execution, approve runs + gate saw the args,
  non-flagged tools skip the gate.
- **Phase 5 — cast / subagents** *(shipped)*: `Cast` of `OperatorRole`s with `Clearance`
  (allow/deny/deny-all); a `SubagentDispatcher` exposes a `send_sidekick` tool so a lead
  delegates a sub-task to a clearance-scoped sidekick that runs as its own agent — only its
  summary returns, transcript isolated. Maps to MAF's handoff pattern. Parity tests:
  clearance rules, cast registry, dispatch + isolation, denied tool unreachable by sidekick.
- **Phase 6 — cost + budgets** *(shipped)*: `CostTracker` (token + USD accounting from
  `UsageDetails` + per-model `ModelPricing`), `CostBudget` (max USD / max tokens) enforced
  mid-run — the loop halts gracefully and `AgentRunResponse.BudgetExceeded` is set. Parity
  tests: pricing math, tracker accumulation, cost tracking with pricing, token + USD budget
  halts.
- **Phase 7 — evals** *(shipped)*: the five shared scenarios (`grounded_answer`,
  `honest_no_knowledge`, `tool_use_supported_answer`, `multi_turn_coherence`,
  `tone_helpfulness`) ported as a gated `SkippableFact` — runs the C# agent over the live
  gateway (`llm.smoo.ai`, an OpenAI-compatible `IChatClient`) + an LLM judge, asserting an
  aggregate mean ≥ 4.0. Gated on `SMOOTH_AGENT_E2E=1` + `SMOOAI_GATEWAY_KEY`, skips cleanly
  otherwise (the judge JSON parser is unit-tested ungated).
- **Phase 8 — packaging** *(shipped)*: `SmooAI.SmoothOperator.Core` published to NuGet;
  releases are Changesets-driven in lockstep with the other artifacts (see below).

> **All phases shipped.** The C# core is the first complete sibling of the Rust reference —
> 31 unit/parity tests + 1 gated live-eval suite. Next sibling (Python/TS) follows the same
> contract.

## Beyond the engine — the service layer (the full system in C#)

The **engine** is one of three layers. A .NET shop that wants the *whole* smooth-operator
system native in C# — not just the in-process agent — needs the layer above the engine, which
is **not yet built in C#**:

| Layer | What | Rust | C# |
| --- | --- | --- | --- |
| Engine | the generic agent framework (loop, tools, memory, checkpoint, cast, cost) | `smooai-smooth-operator-core` | `SmooAI.SmoothOperator.Core` ✅ |
| **Service** | the knowledge-chat **system** on the engine: WS protocol serving, durable storage, ingestion + connectors, ACL, citations, reranker, auth, admin API | `smooth-operator-server` (+ adapters, ingestion) | **— not built —** |
| Client | a protocol client that talks to a running service | — | `SmooAI.SmoothOperator` ✅ (+ `IChatClient` facade) |

A C# **`SmooAI.SmoothOperator.Server`** (the analog of `smooth-operator-server`) would consume
`…Core` and add: an ASP.NET WebSocket host serving the schema-driven protocol; durable adapters
(Postgres+pgvector / DynamoDB) for conversations + knowledge + checkpoints (the engine ships
in-memory); the ingestion pipeline + connectors (GitHub, …); ACL-filtered retrieval
(`Principal` / `AccessContext` / groups); JWT / trusted auth; citations + reranker; the
`/admin/*` API; and a deployable host (container, SST / k8s).

It's a **much larger surface** than the engine, and it's optional — a .NET shop can run the
Rust server + the .NET client, or embed `…Core` directly with its own hosting. The C# service
is the "run the entire system in .NET" option: full native parity, the logical completion of
the polyglot vision.

### Server roadmap (C#)

`SmooAI.SmoothOperator.Server` lives at `dotnet/server`, consumes `…Core`, and is checked
against the **same `spec/` schemas + conformance fixtures** as the Rust server (via the protocol
client's `ProtocolValidator`).

- **Server Phase 0 — protocol runner** *(shipped)*: `ISessionStore` (in-memory) + a `TurnRunner`
  that drives the engine per `send_message` turn (load history → ground in knowledge → stream the
  engine → persist → citations) + a `FrameDispatcher` routing `ping` / `create_conversation_session`
  / `get_session` / `send_message` by `action`. Produces the exact event sequence
  (`immediate_response` 202 → `stream_token`s → `eventual_response` 200, triple-nested data +
  citations), each frame **schema-validated**. 5 conformance tests.
- **Server Phase 1 — WebSocket host** *(shipped)*: `SmooAI.SmoothOperator.Server.AspNetCore` —
  `app.MapSmoothOperatorWebSocket("/ws")` + `AddSmoothOperatorServer()` DI. A channel-backed
  pump (single writer, multi-frame receive) mirrors the Rust sink/writer split.
  **Integration tests** boot the host in-process (TestServer) and drive a **real WebSocket** —
  the C# parity of `rust/.../tests/protocol_smoke.rs` (ping→pong, create_session descriptor with
  echoed UUID agentId, full send_message stream, unknown-action-doesn't-drop-connection).
- **Server Phase 2 — durable storage** *(session store shipped)*: `ISessionStore` is now async
  (matching the Rust `StorageAdapter`); `SmooAI.SmoothOperator.Server.Postgres` adds a
  `PostgresSessionStore` (Npgsql; the `conversation_sessions` + `conversation_messages` tables,
  `CREATE TABLE IF NOT EXISTS`). A **shared `ISessionStore` contract test runs against both the
  in-memory and Postgres adapters** (the Rust adapter-parity pattern), on a real Postgres via
  Testcontainers (Docker-gated, skips cleanly otherwise) — plus a durability test (data survives
  a fresh store instance). The **knowledge adapter** is also shipped: an `IEmbedder` seam (+
  `DeterministicEmbedder`) and a `PostgresKnowledgeBase` (pgvector — embed on ingest, cosine-rank
  on query), with the `IKnowledgeBase` contract asserted against **both** the in-memory (lexical)
  and Postgres (vector) stores. *Still open:* the checkpoint adapter on Postgres.
- **Server Phase 3 — ingestion + connectors** *(shipped)*: `IConnector` (+ `MockConnector`), a
  `Chunker` (overlapping, size-bounded, whitespace-aware), an `IngestPipeline`
  (connector → chunk → embed → store into the `IKnowledgeBase`), and a `GitHubConnector` (lists
  the repo tree, fetches text/code files). The connector contract is asserted against the
  `MockConnector`, and the GitHub connector is unit-tested against a **fake `HttpMessageHandler`**
  (the .NET parity of mocking external APIs) — so it runs in CI without hitting GitHub. End-to-end
  test: GitHub docs → pipeline → queryable answers. *Still open:* wiring an `/admin/connectors/{id}/index`
  trigger (Phase 5).
- **Server Phase 4 — ACL + auth** *(primitives shipped)*: `Principal` / `AccessContext` +
  `TokenAccessResolver` (HS256 JWT verify, trusted base64url identity, **fail-closed** to anonymous
  on absent/malformed/forged/expired). `DocumentAcl` + `AclKnowledgeStore` filter retrieval by the
  caller's groups BEFORE scoring — the C# `knowledge_for_access`. Parity tests mirror the Rust
  `acl_chat_leak` suite (anonymous → public-only, entitled → private, unentitled → no leak) +
  forged/expired JWT fail-closed. *Next:* thread `AccessContext` from the `?token=` slot through
  the dispatcher → runner so the live chat path enforces it end-to-end (+ a Postgres `acl` column).
- **Server Phase 5 — tool/HITL `stream_chunk`s, the reranker, the `/admin/*` API**.
- **Server Phase 6 — deployable host** *(shipped)*: `SmooAI.SmoothOperator.Server.Host` — a
  runnable ASP.NET app that wires the model (OpenAI-compatible gateway), storage (Postgres or
  in-memory), auth (jwt/trusted/none), and **startup GitHub ingestion** (each repo's docs stamped
  with its `github:owner/repo` ACL group) from env config; serves `/health` + `/ws`. A Dockerfile
  ships it as a container. A boot test (WebApplicationFactory) proves it serves `/health` + a WS
  ping with a mocked model (CI-safe). The host uses the durable **`PostgresAclKnowledgeStore`**
  (pgvector + `acl_public`/`acl_groups`, the ACL filtered **in SQL** — `acl_groups && @groups`)
  when a database is configured, with the ACL leak contract asserted against **both** the
  in-memory and Postgres ACL stores. A **`GatewayEmbedder`** (OpenAI-compatible `/embeddings`)
  gives the durable store real semantic vectors when a gateway key is present (deterministic
  fallback otherwise); unit-tested against a fake HTTP handler. *Still open:* the reranker, the
  `/admin/*` API, and a live-gateway integration test.

## Adding the Nth language core

1. Pick the ecosystem's idiomatic agent/LLM abstraction (the language's MEAI-equivalent).
2. Port Phase 0 first — the loop + a mock model + the one parity assertion.
3. Work the phases in order; each must pass the shared parity tests before moving on.
4. Wire the eval scenarios; clear ≥ 4.0.
5. Package for the ecosystem's registry.

## Related

- [[Architecture Overview]] — the system around the engine.
- [[.NET MEAI]] — the .NET *client* surface (the interop skin); this doc is about the *engine*.
- [[Using the Polyglot Clients]] — the protocol-client story (the other axis of "polyglot").
