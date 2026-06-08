# Roadmap

The phased plan for building smooth-operator and getting the smooai monorepo to dogfood it (replacing LangGraph). Phases are roughly sequential but several can overlap. Status legend: ✅ done · 🟡 in progress · ⬜ not started.

## Phase 0 — Foundations (the two repos)

- ✅ Split: `smooth-operator-core` (engine) and `smooth-operator` (service), both public, MIT.
- ✅ **Extract smooth-operator standalone** — carved the Rust crate out of the `smooth` monorepo into `SmooAI/smooth-operator-core`, detached from the workspace, internal couplings feature-gated (`bigsmooth`), secrets redacted. `cargo build` (default/bigsmooth/sqlite) + `cargo test --lib` (408) green.
- ⬜ Publish `smooai-smooth-operator-core` to crates.io; tag `v0.13.x`. *(then smooth-operator switches its path dep to the published crate)*
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

- ✅ Define the `StorageAdapter` trait surface (`rust/smooth-operator/src/adapter.rs`): conversations, participants, messages, sessions, + sync `checkpoints()`/`knowledge()` accessors so smooth-operator's `CheckpointStore`/`KnowledgeBase` plug in unchanged.
- ✅ **In-memory adapter** (`rust/adapters/in-memory`) — the conformance baseline; delegates checkpoints/knowledge to smooth-operator's `MemoryCheckpointStore`/`InMemoryKnowledge`. Integration test green.
- ✅ **Postgres adapter** (`rust/adapters/postgres`, k8s path): conversation/participant/message/session tables; `PostgresCheckpointStore`; `pgvector` (HNSW cosine) + `tsvector` BM25 knowledge with RRF; pluggable `Embedder` (deterministic default + optional `text-embedding-3-small` gateway). Mirrors smooai's `knowledge_vectors`. testcontainers conformance green (pgvector/pgvector:pg16).
- ✅ **DynamoDB adapter** (`rust/adapters/dynamodb`, AWS path): raw aws-sdk single overloaded table (GSI1) for conversations/participants/messages/sessions + `DynamoCheckpointStore` (smooth-operator's sync trait); knowledge = brute-force cosine (testable) + real **S3 Vectors** (`s3-vectors` feature, aws-sdk-s3vectors v1.27). testcontainers conformance green (amazon/dynamodb-local). (modyne adoption = Phase 9.)
- ⬜ Adapter conformance tests run against every backend (the in-memory test is the template).

> **API note:** smooth-operator's `CheckpointStore`/`KnowledgeBase` are **synchronous** traits, and `CheckpointStore` keys on `agent_id` (not `thread_id`). The `Session.thread_id ↔ Checkpoint.agent_id` bridge lives in the Phase 3 runtime.

## Phase 3 — Agent runtime on smooth-operator (`rust/`, then bindings)

- ✅ `KnowledgeChatRuntime` (`rust/smooth-operator/src/runtime.rs`) — runs a real smooth-operator `Agent::run` loop with `with_knowledge` auto-injection + a `knowledge_search` tool over the StorageAdapter. MockLlmClient-tested.
- ✅ **Real-LLM E2E** (`tests/e2e_llm_smoo_ai.rs`, gated on `SMOOTH_AGENT_E2E=1`+`SMOOAI_GATEWAY_KEY`) — live `claude-haiku-4-5` via llm.smoo.ai: plain completion (PONG), streaming deltas, and the headline — the model autonomously invokes `knowledge_search`, retrieves a seeded "17-day return window," and answers "17" (real tool-calling + RAG grounding). 4/4 live.
- 🟡 **Per-session conversation memory** — the E2E surfaced that the runtime builds a fresh `Agent` (new id) per turn, so cross-turn memory misses. Being fixed in the WS service (stable per-session agent id + `with_prior_messages` replay).
- ⬜ Re-express the smooai general-agent pipeline as a smooth-operator `Workflow`: nodes for intake, guardrails, knowledge_search, response_gen, tool_execution, structure_response, escalation, analytics, memory_update.
- ⬜ Wire the real `KnowledgeBase` impl (vector-backed) into the workflow, replacing the in-memory stub.
- ⬜ HITL: write-confirmation + OTP via the `human` module / `ConfirmationHook`, surfaced as protocol events.
- ⬜ Checkpoint per session thread; resume on the next turn.
- ⬜ **OpenTelemetry `gen_ai.*` semantic conventions** in the runtime (and upstream in smooth-operator-core, where OTel is a parity gap) — interops with the smooai monorepo's existing `gen_ai.*` spans and the Microsoft Agent Framework. See [DOTNET.md](DOTNET.md) §6.

## Phase 4 — Tools (`spec/` + runtime)

- ⬜ Port the `ToolDefinition` shape (id, description, `requiresWriteConfirmation`, `defaultAuthLevel`, `createTool`, `isAvailable`) and the registry/resolve flow.
- ⬜ Ship a starter built-in catalog: `knowledge_search`, `web_search`, `fetch_url`, `conversation_history`.
- ⬜ Tool-definition authoring guide so users add tools in their own language.

## Phase 5 — Polyglot clients & service (`typescript/`, `go/`, `dotnet/`, `python/`)

Every client is generated from `spec/` (protocol-first) and validates the shared conformance fixtures. Each has a transport-agnostic native client with `requestId` correlation, a streaming `MessageTurn` (awaitable terminal + iterate `stream_token`/`stream_chunk`), and HITL resume routing.

- ✅ **TypeScript** (`@smooai/smooth-operator`) — Lambda-native; the dogfood target. 16 tests.
- ✅ **C#/.NET** (`SmooAI.SmoothOperator`, net8.0) — first-class. Position-independent `ServerEventConverter` event union. 21 tests.
- ✅ **Go** (`github.com/SmooAI/smooth-operator/go`) — `ServerEvent`+`Raw` accessor pattern. 26 tests (race-clean).
- ✅ **Python** (`smooth_operator`) — pydantic v2 discriminated unions, async client. 26 tests.
- ✅ **Live cross-language E2E** — every client (TS/Go/.NET/Python) boots the real `smooth-operator-server` subprocess (key in env, KB seeded) and drives a real `claude-haiku-4-5` turn over WebSocket: ≥1 streamed event, knowledge-grounded "17", per-session "Zog" memory. Gated on `SMOOTH_AGENT_E2E=1`+`SMOOAI_GATEWAY_KEY`; default suites stay credential-free + skip. Two real bugs the live E2E caught (mocks masked them): .NET `[JsonPolymorphic]` required `type` first but the Rust server emits keys alphabetically (→ custom converter); `agentId` is UUID-typed in `spec/` so pydantic rejects bare strings while Go/TS are lenient — **follow-up: pick string-vs-UUID `agentId` and align all clients.**
- ⬜ Service hosts per language (the Rust server is the reference; TS/Go/.NET/Python service hosts next), and a runnable "hello knowledge-chat" example each.
- ⬜ In-process FFI where it pays off: napi-rs (TS/Lambda), PyO3/uniffi (Python).
- ✅ **.NET ecosystem interop** (shipped; 6 MEAI tests) (see [DOTNET.md](DOTNET.md)): a `SmoothAgentChatClient : IChatClient` facade over the remote client (Microsoft.Extensions.AI), `services.AddSmoothAgent(...)` DI, a `SmoothAgentThread` handle, `AIFunction`-based tool authoring, and middleware mapping to smooth-operator's `ToolHook`. Borrowed from Microsoft Agent Framework idioms.

## Phase 6 — Deploy (`deploy/`)

- ✅ **SST** (`deploy/sst`): API Gateway WebSocket + **Rust Lambda** (`rust/smooth-operator-lambda`, per-message dispatch; Management-API post-back preserving streaming; DynamoDB state) + sst.aws.Dynamo table + S3 bucket + S3 Vectors (raw provider/CLI; brute-force fallback). Verified compile + 47 workspace tests + tsc (NOT deployed — SST v4 has no creds-free synth). cargo-lambda build documented.
- ✅ **Helm + ArgoCD** (`deploy/k8s`): Dockerfile + chart (deployment/service/WS-ingress/hpa/configmap/secret) + ArgoCD Application. helm lint + template + kubectl dry-run green. Server now binds `SMOOTH_AGENT_BIND` (0.0.0.0 in k8s). Expects external pgvector Postgres.
- ⬜ `npx smooth-operator deploy` UX wrapper.
- ⬜ Extract the reusable pieces into a public **`SmooAI/deploy`** package (SST constructs + Helm/ArgoCD) once the first concrete deploy works; dogfood into smooai. See [DEPLOY.md](DEPLOY.md).

## Phase 7 — Dogfood in the smooai monorepo

- ⬜ Replace `packages/backend/src/ai/graphs/**` (LangGraph) with smooth-operator's runtime on smooth-operator.
- ⬜ Point the existing `@smooai/realtime` WebSocket handlers at the smooth-operator protocol.
- ⬜ Keep Postgres/pgvector in smooai; verify retrieval parity (Voyage + hybrid + rerank).
- ⬜ Cut over behind a flag; verify on a customer site.

## Phase 8 — Managed offering (`lom.smoo.ai`)

- ⬜ Stand up the hosted control plane + the SST stack as the multi-tenant backend.
- ⬜ Landing page + docs + self-serve onboarding.

## Phase 9 — Stretch goals

- ⬜ **gRPC + MessagePack transport** (internal/service-to-service) alongside WebSocket+JSON. The protocol is already transport-agnostic (the spec defines messages; each client has a `Transport` interface) — add a gRPC bidi-streaming binding carrying MessagePack-encoded protocol messages, for low-overhead internal hops (e.g. a TS Lambda → Rust engine) where WS+JSON isn't wanted. Browser clients stay on WS+JSON. Stretch, not blocking.
- ⬜ **Adopt `modyne` (the "ElectroDB for Rust") in the DynamoDB adapter** — [modyne](https://github.com/neoeinstein/modyne) (mature, *DynamoDB Book*-aligned) gives ElectroDB-style single-table entity/projection/query ergonomics. The DynamoDB adapter ships first on raw `aws-sdk-dynamodb` (correct baseline), then refactors onto `modyne`. (Build a SmooAI-branded library only if `modyne`/[`deez`](https://github.com/Sife-ops/deez) fall short of our access patterns.)

---

## Phase 10 — Connectors & quality regression (partly done)

- ✅ G1 ingestion framework (Connector/Chunker/pipeline + file/web). ✅ G3 document ACLs. ✅ G4 LLM-judge evals. ✅ G8 rerank. ✅ G5 widget Playwright e2e. ✅ G6 kind deploy smoke.
- ⬜ **GitHub connector** (README/docs/md + code + issues/PRs/discussions; GitHub App installation token / PAT) + a live `github_search` tool — first consumer = `examples/dev-support`.
- ⬜ Connector breadth (the other SaaS sources), per Onyx.

## Phase 11 — Knowledge depth (Onyx parity)

Recurring principle: **"Smoo-powered or bring-your-own"** — hosted lom.smoo.ai wires Smoo's apps (identity, GitHub App, Slack App, managed parsing); self-host brings their own. Same code, two postures.

- 🟡 **Background / incremental indexing** — engine done (`smooth_operator_ingestion::indexing`: `IndexingService::run_once` = `latest_cursor` → `pull(since)` → idempotent `ingest` → `IndexingRun`; `IndexingStore` trait + `InMemoryIndexingStore`; `IndexingProgress` seam for `job_status_updated`; cursor + failure-path + progress tests). See [INDEXING.md](INDEXING.md). Persistent `IndexingStore` adapters (Postgres/DynamoDB `indexing_runs`) ✅ shipped (Phase 12 follow-up — see below). Remaining: the EventBridge Scheduler → Step Functions/Lambda (k8s: CronJob+worker) wiring.
- ⬜ **Structured citations** (do early — sources are already retrieved): `citations[]` (id/title/url/snippet/score) in `eventual_response` + widget rendering.
- ⬜ **Rich file parsing** — a `FileParser` seam: text/md/html inline; PDF/docx/pptx/xlsx+OCR → **Docling** in a container Lambda (off the hot path; feeds the chunker).
- ✅ **Document sets / curation / boosting** — membership side table + `RetrievalFilter` (set scope + metadata equality) + `boost` field re-ranking, applied in our layer (`CuratedKnowledgeStore`, over-fetch→filter→boost→re-sort→truncate), composing with ACL (ACL ∧ set ∧ metadata). Ingest-time tagging via `IngestOptions::in_document_sets`/`with_boost`. See [DOCUMENT-SETS.md](DOCUMENT-SETS.md).
- 🟡 **Query-time features** — ✅ metadata filters (`RetrievalFilter::metadata_eq`, see above). Remaining: LLM query-rephrase, recency/time-decay boost in RRF.

## Phase 12 — Management UI + Auth/RBAC

- ⬜ **Next.js management app** (connector config, document sets, chat history, indexing status, settings) on `sst.aws.Nextjs` (OpenNext→Lambda) or containerized. *(increment 2 — consumes the admin API below.)*
- ✅ **Auth + admin API (increment 1)** — `Role` (admin/curator/basic, ordered) + `Principal` + the `AuthVerifier` seam with three modes: BYO **SST OpenAuth** JWT (`AUTH_MODE=jwt`, HS256 + RS256), **Smoo identity** (`AUTH_MODE=smoo`, hosted; live-introspection stubbed), and dev-only `NoAuthVerifier` (`AUTH_MODE=none`). Secure-by-default (`jwt` default; keyless `jwt`/`smoo` refuses to start — never silent no-auth). Auth-gated admin HTTP API on the axum service (`/admin/me`, `/admin/conversations`(+`/{id}/messages`), `/admin/indexing/runs`, `/admin/document-sets`, public `/admin/health`) with org-scoping + "Basic sees own". See [ADMIN-API.md](ADMIN-API.md). RBAC layers on org scoping + `DocAcl`.
- ✅ **Admin write API (increment 3)** — connector-config CRUD (`GET`/`POST`/`PUT`/`DELETE /admin/connectors[/{id}]`), an **index trigger** (`POST /admin/connectors/{id}/index` builds the connector from its config and runs `IndexingService::run_once`, recording the run in the shared `IndexingStore`), and per-org **agent settings** (`GET`/`PUT /admin/settings`). RBAC: reads are Curator, mutations Admin-only, index trigger Curator; everything org-scoped (cross-org ⇒ 404). The **`auth_ref` secret model** — a connector stores a secret *name*, never the token; resolved from env at index time, never returned in any response. New stores `ConnectorConfigStore`/`SettingsStore` (trait + in-memory) in `smooth-operator/src/`; durable Postgres/DynamoDB impls shipped (see below). See [ADMIN-API.md](ADMIN-API.md).
- ✅ **Persistent admin stores (Phase 12 follow-up)** — the three management-console stores (`ConnectorConfigStore`, `SettingsStore`, `IndexingStore`) are now **durable** on both backends, selected to match the configured storage backend (`SMOOTH_AGENT_STORAGE` = `memory`/`postgres`/`dynamodb`). Postgres: `connector_configs` (PK `(org_id,id)`), `agent_settings` (PK `org_id`), `indexing_runs` (PK `id`, indexed `(connector_name, started_at DESC)`) in `adapters/postgres/src/admin.rs`. DynamoDB single-table: ConnectorConfig `ORG#<org>`/`CONNECTOR#<id>`, AgentSettings `ORG#<org>`/`SETTINGS#`, IndexingRun `IXCONN#<name>`/`<padded started_at>#<id>` in `adapters/dynamodb/src/admin.rs`. Sync traits bridged over the async pool/SDK like `KnowledgeBase`/`CheckpointStore`. Cursor-semantics (`latest_cursor` = max over **succeeded** runs only) + org-isolation preserved; wired via `build_state_from_env_async`. TDD: testcontainers conformance on **both** backends (`pgvector/pgvector:pg16` + `amazon/dynamodb-local`). See [ADMIN-API.md](ADMIN-API.md).

## Phase 13 — Answer bots

- ⬜ **Slack bot** — Slack Events API → Lambda → conversation/session → agent → post back (our model already has the `slack` platform). Hosted = smoo's Slack app; self-host = BYO. Teams/Discord later.

## Phase 14 — Analytics & feedback

- ⬜ Per-turn query log (query, retrieved docs, answer, latency, tokens) + 👍/👎 feedback events + a dashboard in the mgmt UI (OTel already covers traces).

---

### Current focus

Done through Phase 6 (dual deploy), Phase 5 (all clients + live cross-language E2E), Phase 10 majority (ingestion, ACLs, evals, rerank, widget e2e, kind CI), plus tool catalog, OTel, .NET MEAI, `SmooAI/deploy`, `SmooAI/chat-widget`.

Queued: (1) **rename** `smooth-operator`→`smooth-operator-core`, `smooth-operator`→`smooth-operator`; (2) **incredible DX-driven, TDD-forward READMEs** for all packages; (3) the **dev-support agent** (`examples/dev-support` + GitHub connector + full-page chat + citations). Then the Phase 11–14 Onyx-parity build-out, and the user-gated arcs (smooai cutover, crates.io/npm publish).
