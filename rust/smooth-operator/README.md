<p align="center">
  <a href="https://smoo.ai"><img src="https://raw.githubusercontent.com/SmooAI/smooth-operator/main/.github/banner.png" alt="smooth-operator — Polyglot AI agent service. One protocol." width="100%" /></a>
</p>

<p align="center">
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-F49F0A?style=for-the-badge&labelColor=020618" alt="license"></a>
  <a href="https://lom.smoo.ai"><img src="https://img.shields.io/badge/hosted-lom.smoo.ai-FF6B6C?style=for-the-badge&labelColor=020618" alt="lom.smoo.ai"></a>
  <a href="https://crates.io/crates/smooai-smooth-operator"><img src="https://img.shields.io/crates/v/smooai-smooth-operator?style=for-the-badge&labelColor=020618&color=00A6A6" alt="crates.io"></a>
  <a href="https://docs.rs/smooai-smooth-operator"><img src="https://img.shields.io/badge/docs.rs-smooth--operator-00A6A6?style=for-the-badge&labelColor=020618" alt="docs.rs"></a>
</p>

<p align="center">
  <b><code>smooai-smooth-operator</code></b> — the reference core of the <a href="https://github.com/SmooAI/smooth-operator">smooth-operator</a> service: the domain model and the one <code>StorageAdapter</code> seam, sitting on top of the Rust agent engine.
</p>

---

## What is this?

`smooai-smooth-operator` (crate name `smooth_operator`) is the **service-layer reference core** of [smooth-operator](https://github.com/SmooAI/smooth-operator) — the layer that sits on top of [`smooth-operator-core`](https://github.com/SmooAI/smooth-operator-core) (the Rust agent engine) and turns it into a knowledge-chat service. It is what the WebSocket server, the storage adapters, and the ingestion pipeline all build against.

It owns three things:

- **`domain`** — storage-agnostic domain structs (`Conversation`, `Participant`, `Message`, `Session`, `Checkpoint`, `Citation`) that mirror the language-neutral `spec/domain/*.json`.
- **`adapter`** — the single **`StorageAdapter`** seam every backend implements (in-memory, Postgres + pgvector, DynamoDB + S3 Vectors). Its checkpoint/knowledge accessors return the engine's own traits, so the engine plugs straight in. See [`docs/STORAGE.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/STORAGE.md).
- **`runtime`** — a `KnowledgeChatRuntime` that constructs a real smooth-operator `Agent` + `Workflow` and drives a turn end to end (knowledge retrieval → tool-calling → streaming).

It also owns two shared retrieval seams both adapters and ingestion depend on: **`embedding`** (the text→vector `Embedder` + the network-free `DeterministicEmbedder`) and **`rerank`** (the optional post-retrieval `Reranker` stage).

> **This is the operator engine/service crate, not a protocol client.** If you want to *connect to* a running service, reach for one of the five clients — [TypeScript](https://www.npmjs.com/package/@smooai/smooth-operator), [Python](https://pypi.org/project/smooai-smooth-operator/), [Go](https://pkg.go.dev/github.com/SmooAI/smooth-operator/go), [.NET](https://www.nuget.org/packages/SmooAI.SmoothOperator), or the Rust client.

---

## Install

```toml
[dependencies]
smooai-smooth-operator = "1"
```

---

## Embed it

Build a `KnowledgeChatRuntime` over any `StorageAdapter` + an `LlmConfig` (the gateway seam) and drive a turn. The in-memory adapter is the conformance baseline; swap in Postgres or DynamoDB unchanged.

```rust
use std::sync::Arc;
use smooth_operator::{KnowledgeChatRuntime, StorageAdapter};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;

// 1. Pick a storage backend (the seam every adapter implements).
let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());

// 2. Build the knowledge-chat runtime over that storage + an LLM gateway config.
//    It constructs a real smooth-operator Agent + Workflow on `smooth-operator-core`.
let runtime = KnowledgeChatRuntime::new(storage, llm_config);

// The reference server in `smooth-operator-server` does exactly this behind the
// WebSocket protocol: a turn runs knowledge retrieval → tool-calling → streamed tokens.
```

The runtime also exposes builder-style seams — `with_curation`, `with_retrieval_filter`, `with_access_control`, `with_llm_provider`, `with_max_iterations`. Exact signatures (and how `LlmConfig` is wired to a gateway) are on [docs.rs](https://docs.rs/smooai-smooth-operator). For the full ingest → chat path, see the [`smooth-operator-server`](https://github.com/SmooAI/smooth-operator/blob/main/rust/smooth-operator-server) crate and the [`rust/examples/dev-support`](https://github.com/SmooAI/smooth-operator/tree/main/rust/examples/dev-support) example.

---

## Where it sits

```text
smooth-operator-core      the agent engine (Agent · Workflow · Tool · LlmProvider · Memory)
        ▲
        │  consumed by
        │
smooai-smooth-operator    ← THIS CRATE — domain model + StorageAdapter seam + KnowledgeChatRuntime
        ▲
        │  built against by
        │
  adapters (memory · postgres · dynamodb) · ingestion · smooth-operator-server · the 5 clients
```

---

## 🧩 Part of Smoo AI

`smooai-smooth-operator` is built and open-sourced by **[Smoo AI](https://smoo.ai)** — the AI-powered business platform with AI built into every product. It is the heart of the [smooth-operator](https://github.com/SmooAI/smooth-operator) service, which exposes one schema-driven WebSocket protocol implemented by five native clients (TypeScript · Python · Go · .NET · Rust).

- 🧠 **The engine it wraps** — [smooth-operator-core](https://github.com/SmooAI/smooth-operator-core)
- 🌐 **The service** — [smooth-operator](https://github.com/SmooAI/smooth-operator) (protocol, server, clients, AWS/k8s deploy)
- 🧰 **More open source from Smoo AI** — [smoo.ai/open-source](https://smoo.ai/open-source)
- ☁️ **Hosted** — [lom.smoo.ai](https://lom.smoo.ai) runs smooth-operator for you, managed and multi-tenant

## 📄 License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).

---

<p align="center">
  Built by <a href="https://smoo.ai"><strong>Smoo AI</strong></a> — AI built into every product.
</p>
