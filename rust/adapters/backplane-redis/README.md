# smooai-smooth-operator-adapter-backplane-redis

<a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>

The **Redis / Valkey `Backplane`** for [smooth-operator](https://github.com/SmooAI/smooth-operator) — the horizontal scale-out seam.

With more than one replica, an event produced on pod A can't reach a socket on pod B. `RedisBackplane` closes that gap **without changing the trait or any call site**: a per-pod in-memory backplane for local registry + delivery, plus a Redis pub/sub bus for cross-pod fan-out.

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service (one schema-driven WebSocket protocol, five native clients, AWS-serverless or Kubernetes deploy). See the [repository](https://github.com/SmooAI/smooth-operator) for the architecture, protocol, and the other crates.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
