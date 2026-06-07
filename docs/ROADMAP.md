# Roadmap

The phased plan for building smooth-agent and getting the smooai monorepo to dogfood it (replacing LangGraph). Phases are roughly sequential but several can overlap. Status legend: ✅ done · 🟡 in progress · ⬜ not started.

## Phase 0 — Foundations (the two repos)

- ✅ Split: `smooth-operator` (engine) and `smooth-agent` (service), both public, MIT.
- 🟡 **Extract smooth-operator standalone** — carve the Rust crate out of the `smooth` monorepo into `SmooAI/smooth-operator`, detached from the workspace, internal couplings feature-gated (`bigsmooth`), secrets redacted, `cargo build`/`test` green. *(in progress)*
- ⬜ Publish `smooai-smooth-operator` to crates.io; tag `v0.13.x`.
- ⬜ Make the `smooth` monorepo consume the extracted crate as a dependency (the "fully extract" follow-through — touches ~20 dependent crates).

## Phase 1 — The protocol (`spec/`)

The wire protocol is the contract every language client implements. It is lifted from the smooai monorepo's `@smooai/realtime` schemas and made language-neutral.

- ⬜ JSON Schema for the envelope: client→server **actions** (`create_conversation_session`, `send_message`, `get_session`, `get_messages`, `ping`, `confirm_tool_action`, `verify_otp`) and server→client **events** (`immediate_response`, `eventual_response`, `stream_chunk`, `stream_token`, `keepalive`, `write_confirmation_required`, `otp_*`, `error`).
- ⬜ Map `stream_chunk`/`stream_token` onto smooth-operator's `AgentEvent` stream.
- ⬜ Codegen pipeline: JSON Schema → per-language types (TS via json-schema-to-typescript, Go via quicktype, .NET via NJsonSchema, Python via datamodel-code-generator).
- ⬜ Conformance test suite (one set of fixtures, every client validated against it).

See [PROTOCOL.md](PROTOCOL.md).

## Phase 2 — Storage adapters (`adapters/`)

One trait, two backends. See [STORAGE.md](STORAGE.md).

- ⬜ Define the `StorageAdapter` trait surface: conversations, participants, messages, sessions, **checkpoint store**, knowledge store.
- ⬜ **Postgres adapter** (k8s path): conversation/participant/message/session tables; Postgres checkpoint store (smooth-operator already ships `PostgresCheckpointStore`); `pgvector` + `tsvector` knowledge with RRF + rerank. Mirror the smooai `knowledge_vectors` schema.
- ⬜ **DynamoDB adapter** (AWS path): ElectroDB single-table for conversation/participant/message/session/checkpoint; **S3 Vectors** for knowledge embeddings.
- ⬜ Adapter conformance tests (same suite runs against both).

## Phase 3 — Agent runtime on smooth-operator (`rust/`, then bindings)

- ⬜ Re-express the smooai general-agent pipeline as a smooth-operator `Workflow`: nodes for intake, guardrails, knowledge_search, response_gen, tool_execution, structure_response, escalation, analytics, memory_update.
- ⬜ Wire the real `KnowledgeBase` impl (vector-backed) into the workflow, replacing the in-memory stub.
- ⬜ HITL: write-confirmation + OTP via the `human` module / `ConfirmationHook`, surfaced as protocol events.
- ⬜ Checkpoint per session thread; resume on the next turn.

## Phase 4 — Tools (`spec/` + runtime)

- ⬜ Port the `ToolDefinition` shape (id, description, `requiresWriteConfirmation`, `defaultAuthLevel`, `createTool`, `isAvailable`) and the registry/resolve flow.
- ⬜ Ship a starter built-in catalog: `knowledge_search`, `web_search`, `fetch_url`, `conversation_history`.
- ⬜ Tool-definition authoring guide so users add tools in their own language.

## Phase 5 — Polyglot clients & service (`typescript/`, `go/`, `dotnet/`, `python/`)

- ⬜ **TypeScript** first (Lambda-native; this is what the smooai monorepo dogfoods). napi-rs in-process embedding of smooth-operator where it pays off.
- ⬜ **C#/.NET** — first-class. Native protocol client + service host (ASP.NET/minimal API + WS).
- ⬜ **Go** — native protocol client + service.
- ⬜ **Python** — native protocol client; PyO3/uniffi in-process embedding optional.
- ⬜ Each language: client conformance + a runnable "hello knowledge-chat" example.

## Phase 6 — Deploy (`deploy/`)

- ⬜ **SST** (`deploy/sst`): API Gateway WebSocket + Lambda handlers (`$connect`, `send_message`, …) + DynamoDB table + S3 Vectors + S3 blob bucket. One-command `deploy`.
- ⬜ **Helm** (`deploy/k8s`): service + Postgres + pgvector + ingress. One-command `helm install`.
- ⬜ `npx smooth-agent deploy` UX wrapper.

## Phase 7 — Dogfood in the smooai monorepo

- ⬜ Replace `packages/backend/src/ai/graphs/**` (LangGraph) with smooth-agent's runtime on smooth-operator.
- ⬜ Point the existing `@smooai/realtime` WebSocket handlers at the smooth-agent protocol.
- ⬜ Keep Postgres/pgvector in smooai; verify retrieval parity (Voyage + hybrid + rerank).
- ⬜ Cut over behind a flag; verify on a customer site.

## Phase 8 — Managed offering (`lom.smoo.ai`)

- ⬜ Stand up the hosted control plane + the SST stack as the multi-tenant backend.
- ⬜ Landing page + docs + self-serve onboarding.

---

### Current focus

Phase 0 (smooth-operator extraction) → Phase 1 (protocol) → Phase 2 (adapters). The first end-to-end milestone is a **Rust reference service** that: accepts a `send_message` over WS, runs a smooth-operator workflow with knowledge retrieval + one tool, streams `AgentEvent`s back as protocol events, and persists to **both** Postgres and DynamoDB adapters.
