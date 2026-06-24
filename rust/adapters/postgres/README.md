# smooai-smooth-operator-adapter-postgres

<a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>

The **Postgres + pgvector `StorageAdapter`** for [smooth-operator](https://github.com/SmooAI/smooth-operator) — the dogfood backend.

The production Postgres implementation of the one storage seam: async OLTP CRUD over a `deadpool-postgres` pool, a `PostgresCheckpointStore`, and pgvector-backed hybrid retrieval. It mirrors the SmooAI monorepo's schema so dogfooding is a swap, not a rewrite. See [`docs/STORAGE.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/STORAGE.md).

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service (one schema-driven WebSocket protocol, five native clients, AWS-serverless or Kubernetes deploy). See the [repository](https://github.com/SmooAI/smooth-operator) for the architecture, protocol, and the other crates.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
