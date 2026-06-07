# `deploy/` — deployment targets

Both paths are first-class. The storage adapter is what makes one codebase deploy to either.

- `sst/` — **AWS serverless** (default, cloud-codable). API Gateway WebSocket + Lambda handlers + DynamoDB (ElectroDB) + S3 Vectors + S3 blobs. One command: `npx smooth-agent deploy`.
- `k8s/` — **Kubernetes / self-host**. Helm chart: service + Postgres + pgvector + ingress. One command: `helm install smooth-agent ./deploy/k8s`.

See [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) §6 for the target matrix.
