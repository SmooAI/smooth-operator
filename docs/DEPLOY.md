# Deployment — dual SST (AWS) / k8s (ArgoCD + Helm)

smooth-agent ships **two first-class deploy paths** from one codebase, with the storage adapter as the seam that makes it possible. AWS-serverless is the default ("cloud-codable by default"); Kubernetes is the self-host path.

| | AWS serverless (default) | Kubernetes / self-host |
| --- | --- | --- |
| IaC | **SST** (`deploy/sst`) | **Helm + ArgoCD** (`deploy/k8s`) |
| Transport | API Gateway WebSocket | Ingress + WS |
| Compute | Lambda | Deployment/pods |
| OLTP | DynamoDB (ElectroDB) | Postgres |
| Vectors | S3 Vectors | pgvector |
| Checkpoints | DynamoDB | Postgres |
| Blobs | S3 | S3-compatible |

## The shared `SmooAI/deploy` package (planned)

The deploy primitives are reused by **two consumers** — smooth-agent and the smooai monorepo (dogfood) — which is the bar for a shared package. To avoid premature abstraction, we **build the concrete deploy in `smooth-agent/deploy` first** (real, deployable, tested against the smooth-agent service), designed to be extractable, then **lift the reusable pieces into a public `SmooAI/deploy`** once the pattern is proven. smooai already has battle-tested SST constructs (`infra/`) and k8s/ArgoCD charts (`k8s/charts`) we harvest from rather than invent.

`SmooAI/deploy` will expose two surfaces:

1. **SST constructs (TypeScript)** — reusable SST v4 components, parameterized:
   - `SmoothAgentApi` — API Gateway WebSocket API + the route Lambda handlers (`$connect`, `send_message`, …) + DynamoDB single table + S3 Vectors index + S3 blob bucket + the `@smooai/config` layer wiring.
   - Smaller building blocks (`WebSocketApi`, `DynamoSingleTable`, `S3VectorsIndex`) so smooai can adopt them piecemeal.
2. **Helm chart + ArgoCD Application (k8s)** — the smooth-agent service + Postgres + pgvector + ingress, with an ArgoCD `Application` manifest and image-tag wiring (matching the smooai api-prime/ArgoCD pattern).

### Consumption
- `smooth-agent/deploy/sst` re-exports/composes the SST constructs; `npx smooth-agent deploy` is the UX wrapper.
- `smooth-agent/deploy/k8s` is the Helm chart + ArgoCD app.
- **Dogfood**: smooai migrates its relevant infra onto the shared constructs once they're proven here.

## Status
Skeleton dirs in place (`deploy/sst`, `deploy/k8s`). Concrete SST stack + Helm chart are tracked in [ROADMAP.md](ROADMAP.md) Phase 6; the `SmooAI/deploy` extraction follows once the first concrete deploy works end-to-end.
