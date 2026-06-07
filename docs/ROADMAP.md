# Roadmap

The phased plan for building smooth-agent and getting the smooai monorepo to dogfood it (replacing LangGraph). Phases are roughly sequential but several can overlap. Status legend: ✅ done · 🟡 in progress · ⬜ not started.

## Phase 0 — Foundations (the two repos)

- ✅ Split: `smooth-operator` (engine) and `smooth-agent` (service), both public, MIT.
- ✅ **Extract smooth-operator standalone** — carved the Rust crate out of the `smooth` monorepo into `SmooAI/smooth-operator`, detached from the workspace, internal couplings feature-gated (`bigsmooth`), secrets redacted. `cargo build` (default/bigsmooth/sqlite) + `cargo test --lib` (408) green.
- ⬜ Publish `smooai-smooth-operator` to crates.io; tag `v0.13.x`. *(then smooth-agent switches its path dep to the published crate)*
- ⬜ Make the `smooth` monorepo consume the extracted crate as a dependency (the "fully extract" follow-through — touches ~20 dependent crates).

## Phase 1 — The protocol (`spec/`)

The wire protocol is the contract every language client implements. It is lifted from the smooai monorepo's `@smooai/realtime` schemas and made language-neutral.

- ✅ JSON Schema (draft 2020-12) for the envelope, **actions** (`create_conversation_session`, `send_message`, `get_session`, `get_messages`, `ping`, `confirm_tool_action`, `verify_otp`) and **events** (`immediate_response`, `eventual_response`, `stream_chunk`, `stream_token`, `keepalive`, `write_confirmation_required`, `otp_*`, `error`, `pong`). In `spec/`. ajv-validated (25 schemas).
- ✅ Domain schemas (`conversation`, `participant`, `message`, `session`, `checkpoint`) in `spec/domain/`.
- ✅ Conformance fixtures (`spec/conformance/fixtures.json`, 5 instances, ajv-validated).
- 🟡 Map `stream_chunk`/`stream_token` onto smooth-operator's `AgentEvent` stream. *(documented in PROTOCOL.md; wired in Phase 3)*
- ⬜ Codegen pipeline: JSON Schema → per-language types (TS via json-schema-to-typescript, Go via quicktype, .NET via NJsonSchema, Python via datamodel-code-generator). *(commands in `spec/codegen/`)*

See [PROTOCOL.md](PROTOCOL.md).

## Phase 2 — Storage adapters (`adapters/`)

One trait, two backends. See [STORAGE.md](STORAGE.md).

- ✅ Define the `StorageAdapter` trait surface (`rust/smooth-agent-core/src/adapter.rs`): conversations, participants, messages, sessions, + sync `checkpoints()`/`knowledge()` accessors so smooth-operator's `CheckpointStore`/`KnowledgeBase` plug in unchanged.
- ✅ **In-memory adapter** (`rust/adapters/in-memory`) — the conformance baseline; delegates checkpoints/knowledge to smooth-operator's `MemoryCheckpointStore`/`InMemoryKnowledge`. Integration test green.
- ⬜ **Postgres adapter** (k8s path): conversation/participant/message/session tables; Postgres checkpoint store (smooth-operator ships `PostgresCheckpointStore`); `pgvector` + `tsvector` knowledge with RRF + rerank. Mirror the smooai `knowledge_vectors` schema.
- ⬜ **DynamoDB adapter** (AWS path): ElectroDB single-table for conversation/participant/message/session/checkpoint; **S3 Vectors** for knowledge embeddings.
- ⬜ Adapter conformance tests run against every backend (the in-memory test is the template).

> **API note:** smooth-operator's `CheckpointStore`/`KnowledgeBase` are **synchronous** traits, and `CheckpointStore` keys on `agent_id` (not `thread_id`). The `Session.thread_id ↔ Checkpoint.agent_id` bridge lives in the Phase 3 runtime.

## Phase 3 — Agent runtime on smooth-operator (`rust/`, then bindings)

- 🟡 `AgentRuntime` skeleton (`rust/smooth-agent-core/src/runtime.rs`) — constructs a real smooth-operator `Agent` + `Workflow`, `with_storage()` wires the adapter's knowledge/checkpoint accessors. *(proof-of-consumption done; real pipeline next)*
- ⬜ Re-express the smooai general-agent pipeline as a smooth-operator `Workflow`: nodes for intake, guardrails, knowledge_search, response_gen, tool_execution, structure_response, escalation, analytics, memory_update.
- ⬜ Wire the real `KnowledgeBase` impl (vector-backed) into the workflow, replacing the in-memory stub.
- ⬜ HITL: write-confirmation + OTP via the `human` module / `ConfirmationHook`, surfaced as protocol events.
- ⬜ Checkpoint per session thread; resume on the next turn.

## Phase 4 — Tools (`spec/` + runtime)

- ⬜ Port the `ToolDefinition` shape (id, description, `requiresWriteConfirmation`, `defaultAuthLevel`, `createTool`, `isAvailable`) and the registry/resolve flow.
- ⬜ Ship a starter built-in catalog: `knowledge_search`, `web_search`, `fetch_url`, `conversation_history`.
- ⬜ Tool-definition authoring guide so users add tools in their own language.

## Phase 5 — Polyglot clients & service (`typescript/`, `go/`, `dotnet/`, `python/`)

Every client is generated from `spec/` (protocol-first) and validates the shared conformance fixtures. Each has a transport-agnostic native client with `requestId` correlation, a streaming `MessageTurn` (awaitable terminal + iterate `stream_token`/`stream_chunk`), and HITL resume routing.

- ✅ **TypeScript** (`@smooai/smooth-agent`) — Lambda-native; the dogfood target. 16 tests.
- ✅ **C#/.NET** (`SmooAI.SmoothAgent`, net8.0) — first-class. System.Text.Json polymorphic event union. 21 tests.
- ✅ **Go** (`github.com/SmooAI/smooth-agent/go`) — `ServerEvent`+`Raw` accessor pattern. 26 tests (race-clean).
- ✅ **Python** (`smooth_agent`) — pydantic v2 discriminated unions, async client. 26 tests.
- ⬜ Service hosts per language (WS server + agent runtime), and a runnable "hello knowledge-chat" example each.
- ⬜ In-process FFI where it pays off: napi-rs (TS/Lambda), PyO3/uniffi (Python).

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

Done: Phase 0 (extraction), Phase 1 (protocol spec), Phase 2 (adapter trait + in-memory), Phase 3 skeleton (runtime consuming smooth-operator), Phase 5 (all five polyglot clients green).

Next: the **first end-to-end knowledge-chat turn** — make the runtime a real smooth-operator `Workflow` (knowledge_search → response_gen → tool_execution), retrieving from the StorageAdapter's `KnowledgeBase`, tested with smooth-operator's `MockLlmClient` (no API keys). Build it on the in-memory adapter first, then add the Postgres and DynamoDB backends, then wrap it in a WS service + the SST/k8s deploys.
