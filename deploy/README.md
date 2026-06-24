# `deploy/` — deployment targets

All three paths are first-class. The storage adapter (and the in-memory/Redis/NATS backplane + auth seams) is what makes one codebase deploy to any of them.

- `local/` — **local / embed-in-process** (laptop dev + the smooth daemon). Everything in-memory, auth off, no external services. One command: `cargo run -p smooai-smooth-operator-server`, or embed via `serve_local`.
- `sst/` — **AWS serverless** (default, cloud-codable). API Gateway WebSocket + Lambda handlers + DynamoDB (ElectroDB) + S3 Vectors + S3 blobs. One command: `npx smooth-operator deploy`.
- `k8s/` — **Kubernetes / self-host**. Helm chart: service + Postgres + pgvector + ingress. One command: `helm install smooth-operator ./deploy/k8s`.

See [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) §6 for the target matrix.
