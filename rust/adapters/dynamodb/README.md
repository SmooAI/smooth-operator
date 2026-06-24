# smooai-smooth-operator-adapter-dynamodb

<a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>

The **DynamoDB single-table `StorageAdapter`** for [smooth-operator](https://github.com/SmooAI/smooth-operator) — the AWS-serverless backend.

One overloaded `PK`/`SK` table plus two GSIs serve every access pattern over `aws-sdk-dynamodb`, with the **same observable semantics** as the in-memory and Postgres baselines (conversation idempotency, external-id participant resolve, cursor paging). Knowledge retrieval rides S3 Vectors (or brute-force DynamoDB). See [`docs/STORAGE.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/STORAGE.md).

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service (one schema-driven WebSocket protocol, five native clients, AWS-serverless or Kubernetes deploy). See the [repository](https://github.com/SmooAI/smooth-operator) for the architecture, protocol, and the other crates.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
