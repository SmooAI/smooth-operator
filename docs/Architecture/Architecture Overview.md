# Architecture

smooth-operator is the **service layer** on top of [`smooth-operator`](https://github.com/SmooAI/smooth-operator-core) (the agent engine). This document describes how the pieces fit, what we borrowed from mature knowledge platforms, and why the design is serverless-first.

## 1. The big picture

```
                    ┌──────────────────────────────────────────────┐
   WebSocket /      │                  smooth-operator                 │
   HTTP client  ───▶│                                              │
  (browser, app,    │  ┌───────────┐   ┌──────────────────────┐    │
   chat widget)     │  │ Protocol  │──▶│   Agent Runtime       │    │
                    │  │ (schema-  │   │  (a smooth-operator   │    │
                    │  │  driven   │   │   Workflow, NOT       │    │
                    │  │  WS)      │   │   LangGraph)          │    │
                    │  └───────────┘   └──────────┬───────────┘    │
                    │                             │                │
                    │   ┌─────────────────────────┼──────────────┐ │
                    │   │      Storage Adapter (trait)            │ │
                    │   │  ┌──────────────┐   ┌────────────────┐  │ │
                    │   │  │ Conversations│   │  Knowledge     │  │ │
                    │   │  │ Participants │   │  (hybrid       │  │ │
                    │   │  │ Messages     │   │   retrieval)   │  │ │
                    │   │  │ Sessions     │   │                │  │ │
                    │   │  │ Checkpoints  │   │                │  │ │
                    │   │  └──────────────┘   └────────────────┘  │ │
                    │   └─────────────────────────────────────────┘ │
                    └──────────────────────────────────────────────┘
                              │                         │
                ┌─────────────┴───────┐   ┌─────────────┴──────────────┐
                │  AWS serverless     │   │  Kubernetes / self-host    │
                │  DynamoDB (aws-sdk) │   │  Postgres                  │
                │  + S3 Vectors       │   │  + pgvector                │
                └─────────────────────┘   └────────────────────────────┘
```

The **only** thing a client ever sees is the [[Protocol Reference]]. Everything behind it — which language the service runs in, which storage backend is wired up, whether the agent core is embedded in-process or called as an engine — is swappable.

## 2. How it consumes smooth-operator-core

The smooai monorepo today runs its agent on **LangGraph** (TypeScript) — a `StateGraph` with nodes `intake_bootstrap → guardrails → knowledge_search ↔ response_gen ↔ tool_execution → structure_response → escalation → analytics → memory_update`, checkpointed with `PostgresSaver`.

smooth-operator **replaces LangGraph with smooth-operator**. The mapping is direct because smooth-operator already ships the analogous primitives:

| LangGraph (smooai today) | smooth-operator (smooth-operator) |
| ------------------------ | ------------------------------ |
| `StateGraph` / `Annotation.Root` | `Workflow<S>` / `WorkflowBuilder<S>` |
| graph node | `Node<S>` (or `FnNode<S>`) |
| conditional edge | `EdgeTarget::Conditional` |
| `PostgresSaver` checkpointer | `CheckpointStore` (Memory/File/SQLite/**Postgres** impls ship today; **DynamoDB** impl added here) |
| `PostgresStore` long-term memory | `Memory` trait |
| tool bound to model | `Tool` trait + `ToolRegistry` |
| streaming `stream_chunk` events | `AgentEvent` stream (`Started`, `LlmRequest`, `ToolCallStart/Complete`, `TokenDelta`, `Completed`, `HumanInputRequired`, …) |
| HITL write-confirmation / OTP pause | `human` module — `HumanRequest::Confirm`, `ConfirmationHook` (`ToolHook::pre_call`) |
| Voyage embeddings + pgvector retrieval | `KnowledgeBase` trait (smooth-operator-core provides the real vector-backed impl; the crate ships an in-memory stub) |

The agent pipeline (the nine "nodes") is re-expressed as a smooth-operator `Workflow`. See [[Roadmap]] Phase 3.

## 3. Conversation / participant model

Lifted from the smooai monorepo's Drizzle schema (the north star), made storage-agnostic:

- **Conversation** — `id`, `platform` (web/sms/email/discord/phone/slack/whatsapp/…), `organizationId`, metadata.
- **Participant** — three-way discriminated type: `user` | `ai-agent` | `human-agent`. `ai-agent` participants carry the agent id; `user` participants optionally carry an external auth id and a CRM contact id.
- **Message** — `direction` (`inbound`/`outbound`), `content` (`{ items: [{type, text}] }`), `from`/`to` participant ids, analytics.
- **Session** — one live "thread": bridges a conversation to a smooth-operator checkpoint thread (replacing `langGraphThreadId`). Tracks status, token/message counts, rate-limit window.
- **Checkpoint** — durable agent state per thread (smooth-operator `Checkpoint`).

Storage-backend mappings are in [[Storage Adapters]].

## 4. Knowledge & retrieval

We use a standard **hybrid retrieval** (the part worth keeping) without Vespa (the part that doesn't fit serverless):

- **Dense**: embedding similarity. Embeddings via a pluggable provider — **Voyage** (`voyage-3-large`, 1024-dim, asymmetric query/document input types), OpenAI, etc.
- **Sparse**: keyword/BM25.
- **Fusion**: Reciprocal Rank Fusion of the two rankings.
- **Rerank** (optional): cross-encoder rerank (e.g. Cohere) of the fused top-K.

Backends:
- **k8s/self-host**: Postgres + `pgvector` (HNSW) + `tsvector` BM25 — mirrors the smooai monorepo's `knowledge_vectors` table exactly.
- **AWS serverless**: **Amazon S3 Vectors** for dense ANN (DynamoDB has no native vector/ANN index), with sparse/keyword handled by a DynamoDB-friendly inverted index or deferred to a managed search service. See [[Storage Adapters]] §Knowledge.

## 5. What we borrowed (and what we didn't)

**Emulate:**
- Hybrid (vector + keyword) retrieval pipeline with reranking.
- The clean decomposition: **Chat · RAG/Knowledge · Agents · Actions(Tools)**.
- Connector-style ingestion (pluggable document sources).
- MIT license, batteries-included self-host story.

**Avoid (poor serverless fit):**
- **Vespa** — replaced by pgvector (k8s) / S3 Vectors (AWS).
- **Persistent Redis & MinIO** — connection state goes to DynamoDB/Redis-optional; blobs go to S3.
- **Long-running Celery workers** — ingestion runs as event-driven Lambda/Step Functions (AWS) or Jobs (k8s), not a standing worker fleet.

## 6. Deploy targets

| | AWS serverless (default) | Kubernetes / self-host |
| --- | --- | --- |
| Transport | API Gateway WebSocket | Ingress + WS |
| Compute | Lambda | Deployment/pods |
| OLTP | DynamoDB (`aws-sdk-dynamodb` single-table) | Postgres |
| Vectors | S3 Vectors | pgvector |
| Checkpoints | DynamoDB | Postgres |
| Blobs | S3 | S3-compatible |
| IaC | SST (`deploy/sst`) | Helm (`deploy/k8s`) |

Both paths are first-class and tested. The storage adapter is the seam that makes this possible — application and agent code never name a backend.

## 7. Polyglot strategy

`.NET` is a first-class target and the agent core is async + streaming-heavy. FFI codegen for .NET/Go is immature for async streaming (uniffi-bindgen-cs is young; csbindgen has no async; UniFFI has open async-trait bugs). So the spine is **protocol-first**: [`spec/`](../../spec) defines the wire protocol once, and each language ships an idiomatic native client. In-process FFI (napi-rs for TS, PyO3/uniffi for Python) is layered on **only where embedding the engine in-process pays off** — never as the only way to use a language. See [smooth-operator's bindings strategy](https://github.com/SmooAI/smooth-operator-core) and [[Roadmap]] Phase 5.
