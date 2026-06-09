# Deployment — dual SST (AWS) / k8s (ArgoCD + Helm)

smooth-operator ships **two first-class deploy paths** from one codebase, with the storage adapter as the seam that makes it possible. AWS-serverless is the default ("cloud-codable by default"); Kubernetes is the self-host path.

| | AWS serverless (default) | Kubernetes / self-host |
| --- | --- | --- |
| IaC | **SST** (`deploy/sst`) | **Helm + ArgoCD** (`deploy/k8s`) |
| Transport | API Gateway WebSocket | Ingress + WS |
| Compute | Lambda | Deployment/pods |
| OLTP | DynamoDB (`aws-sdk-dynamodb`) | Postgres |
| Vectors | S3 Vectors | pgvector |
| Checkpoints | DynamoDB | Postgres |
| Blobs | S3 | S3-compatible |

## The shared `SmooAI/deploy` package (extracted ✅)

The deploy primitives are reused by **two consumers** — smooth-operator and the smooai monorepo (dogfood) — which is the bar for a shared package. The concrete deploy was built in `smooth-operator/deploy` first (real, designed to be extractable), then the reusable pieces were **lifted into the public [`SmooAI/deploy`](https://github.com/SmooAI/deploy)** package. `smooth-operator` now **consumes** that package rather than carrying inline resources.

`SmooAI/deploy` exposes two surfaces:

1. **SST constructs (TypeScript, `@smooai/deploy`)** — reusable SST v4 components, parameterized:
   - `SmoothAgentApi` (class) — API Gateway WebSocket API + the route Lambda handlers (`$connect`, `$disconnect`, `send_message`, `ping`, `$default`) + DynamoDB single table + S3 blob bucket + S3 Vectors env wiring (placeholder for the not-yet-native SST S3 Vectors component) + the gateway-key secret + IAM links.
   - Smaller building blocks (`WebSocketLambdaApi`, `DynamoSingleTable`) so smooai can adopt them piecemeal.
2. **Helm chart + ArgoCD Application (`helm/smooth-operator`)** — the smooth-operator service + (external pgvector) Postgres + WebSocket ingress, with a templated ArgoCD `Application` manifest and image-tag wiring (matching the smooai api-prime/ArgoCD pattern).

### Consumption
- **`smooth-operator/deploy/sst`** consumes `@smooai/deploy` via `new SmoothAgentApi(...)`. The dependency is a local **path dep** today (`"@smooai/deploy": "file:../../../deploy/sst"`, a sibling `SmooAI/deploy` checkout); the npm-publish follow-up (path dep → published `@smooai/deploy`) is tracked in [`SmooAI/deploy/docs/Consuming.md`](https://github.com/SmooAI/deploy/blob/main/docs/Consuming.md#publish-follow-up).
- **`smooth-operator/deploy/k8s`** is retained as a self-contained deployable mirror; the canonical chart is now `SmooAI/deploy`'s `helm/smooth-operator`, and `deploy/k8s/README.md` documents the thin-overlay / chart-dependency form for consuming it.
- **Dogfood**: smooai migrates its relevant infra onto the shared constructs/chart piecemeal.

## Status
Extracted. `SmooAI/deploy` holds the `@smooai/deploy` SST constructs + the `smooth-operator` Helm chart; `smooth-operator/deploy/sst` consumes the construct. Verification is `tsc --noEmit` (both the package and the consuming config) + `helm lint`/`helm template`. The npm/OCI publish of the package + chart is the remaining follow-up (see `SmooAI/deploy/docs/Consuming.md`).

---

**In this vault:** [[Home]] · [[Self-Hosting]] · [[Storage Adapters]] · [[Architecture Overview]] · [[Configuration]]
