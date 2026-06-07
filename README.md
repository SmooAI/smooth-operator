# smooth-agent

**An open-source, cloud-codable AI agent service** — knowledge chat, tools, and multi-participant conversations — built on [`@smooai/smooth-operator`](https://github.com/SmooAI/smooth-operator).

Think of it as an [Onyx](https://github.com/onyx-dot-app/onyx)-class knowledge-assistant platform, but **serverless-first** and **polyglot**: the agent orchestration core is Rust (smooth-operator), and the service speaks one schema-driven WebSocket protocol that every language client implements natively.

> **Status: scaffolding.** This repo is being built in the open. See [`docs/ROADMAP.md`](docs/ROADMAP.md) for the phased plan and what works today.

---

## Why this exists

Most open agent platforms (Onyx/Danswer, etc.) are wonderful but **stateful and container-bound** — Postgres + a dedicated vector engine (Vespa) + Redis + a blob store + long-running Celery workers. That's a poor fit for stateless serverless and an awkward thing to "just deploy."

smooth-agent makes a different bet:

- **Cloud-codable by default.** `npx smooth-agent deploy` (SST) stands up an entire agent backend on AWS — API Gateway WebSockets + Lambda + DynamoDB + S3 Vectors — no servers to babysit. A Helm chart stands up the same thing on Kubernetes with Postgres + pgvector.
- **One agent core, every language.** Orchestration (agents, workflows, tools, checkpointing, memory, HITL, cost) lives once in [`smooth-operator`](https://github.com/SmooAI/smooth-operator) (Rust). TypeScript, Go, C#/.NET, and Python talk to it over a stable wire protocol — so you write your tools and your service in the language you already use.
- **Conversations are first-class.** A conversation has many **participants** (`user`, `ai-agent`, `human-agent`), many messages, sessions, and durable agent checkpoints. Human handoff and HITL approvals are built in, not bolted on.
- **Hybrid retrieval, pluggable embeddings.** Dense (vector) + sparse (keyword/BM25) + optional rerank. [Voyage AI](https://www.voyageai.com/) embeddings supported out of the box, alongside OpenAI and others.

## The two-repo split

| Repo | What it is |
| ---- | ---------- |
| [`smooth-operator`](https://github.com/SmooAI/smooth-operator) | The polyglot **agent orchestration core** — `Agent`, `Workflow`, `Tool`, `CheckpointStore`, `LlmProvider`, `Memory`, `KnowledgeBase`. Rust reference + TS/Go/.NET/Python bindings. The engine. |
| **`smooth-agent`** (this repo) | The **service** that turns the engine into a product — conversations, knowledge ingestion + retrieval, a tool catalog, the WebSocket protocol, and the AWS/k8s deploy paths. |

## Run it hosted

Don't want to operate it yourself? **[lom.smoo.ai](https://lom.smoo.ai)** runs smooth-agent as a managed service.

## Repository layout

```
smooth-agent/
├── spec/            # The language-neutral wire protocol (JSON Schema) — source of truth for all clients
├── rust/            # Reference service implementation (Rust)
├── typescript/      # TypeScript service + client (Lambda-native; what the smooai monorepo dogfoods)
├── go/              # Go client + service
├── dotnet/          # C#/.NET client + service
├── python/          # Python client + service
├── adapters/        # Storage adapters: postgres (pgvector) and dynamodb (ElectroDB + S3 Vectors)
├── deploy/
│   ├── sst/         # AWS serverless deploy (API GW WebSockets + Lambda + DynamoDB + S3 Vectors)
│   └── k8s/         # Helm chart (Postgres + pgvector)
└── docs/            # Architecture, roadmap, protocol, storage design
```

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — system design, the agent pipeline, how it consumes smooth-operator
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — phased build plan and current status
- [`docs/PROTOCOL.md`](docs/PROTOCOL.md) — the schema-driven WebSocket protocol
- [`docs/STORAGE.md`](docs/STORAGE.md) — the storage adapter trait, Postgres and DynamoDB/ElectroDB/S3 Vectors designs

## License

MIT © 2026 Smoo AI
