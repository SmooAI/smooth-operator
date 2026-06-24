# smooai-smooth-operator-server

<a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>

The **reference WebSocket service** for [smooth-operator](https://github.com/SmooAI/smooth-operator) — the deployment surface.

It speaks the schema-driven protocol in [`spec/`](https://github.com/SmooAI/smooth-operator/tree/main/spec) over a smooth-operator-backed `KnowledgeChatRuntime`, so the generated TypeScript / Python / Go / .NET / Rust clients connect and drive real LLM turns unmodified. Run it locally with `cargo run -p smooai-smooth-operator-server` (in-memory storage, no database to provision), or deploy it three ways from the one codebase:

- **Local** — `cargo run`, in-memory adapter, for development and the cross-language E2E.
- **Kubernetes** — Helm + ArgoCD, Postgres + pgvector, the Redis/NATS backplane for multi-pod scale-out.
- **AWS serverless (SST)** — the companion [`smooth-operator-lambda`](https://github.com/SmooAI/smooth-operator/tree/main/rust/smooth-operator-lambda) crate behind API Gateway WebSocket, with DynamoDB + S3 Vectors.

See [`docs/DEPLOY.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/DEPLOY.md) for the full matrix.

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service (one schema-driven WebSocket protocol, five native clients, AWS-serverless or Kubernetes deploy). See the [repository](https://github.com/SmooAI/smooth-operator) for the architecture, protocol, and the other crates.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
