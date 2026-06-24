# smooai-smooth-operator-ingestion

<a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>

**Knowledge ingestion + connectors** for [smooth-operator](https://github.com/SmooAI/smooth-operator).

The pipeline that pulls documents from a source (file / web), chunks them, embeds them, and stores them in the `StorageAdapter` knowledge slice so they become retrievable. See [`docs/INGESTION.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/INGESTION.md).

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service (one schema-driven WebSocket protocol, five native clients, AWS-serverless or Kubernetes deploy). See the [repository](https://github.com/SmooAI/smooth-operator) for the architecture, protocol, and the other crates.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
