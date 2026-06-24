# smooai-smooth-operator-adapter-backplane-nats

<a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>

The **NATS `Backplane`** for [smooth-operator](https://github.com/SmooAI/smooth-operator) — cross-pod scale-out + a shared event bus.

Same shape as the Redis backplane (per-pod local delivery + a cross-pod bus) but over NATS subjects — which adds queue groups, JetStream persistence/replay, and a broker that doubles as the platform's multi-channel event bus, so non-AI publishers (job status, ingestion progress, notifications) can share it.

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service (one schema-driven WebSocket protocol, five native clients, AWS-serverless or Kubernetes deploy). See the [repository](https://github.com/SmooAI/smooth-operator) for the architecture, protocol, and the other crates.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
