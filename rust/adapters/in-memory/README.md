# smooai-smooth-operator-adapter-memory

<a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>

The **in-memory `StorageAdapter`** for [smooth-operator](https://github.com/SmooAI/smooth-operator) — the conformance / test baseline.

All OLTP slices (conversations, participants, messages, sessions) live in `HashMap`s behind a single `RwLock`; checkpoints and knowledge delegate to smooth-operator's own in-memory stores — exactly what the engine expects. A faithful (if non-durable) stand-in for the Postgres / DynamoDB backends, and the reference server's default storage.

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service (one schema-driven WebSocket protocol, five native clients, AWS-serverless or Kubernetes deploy). See the [repository](https://github.com/SmooAI/smooth-operator) for the architecture, protocol, and the other crates.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
