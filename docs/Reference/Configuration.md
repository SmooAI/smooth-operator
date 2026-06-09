# Configuration

Every environment variable / config key for the reference `smooth-operator-server`
and Lambda, in one place. The server is configured entirely by env vars
(`smooth-operator-server/src/config.rs`); when deployed in the SmooAI monorepo,
secrets come from `@smooai/config` rather than raw env vars (the key **names** are
the same).

> **Secrets policy.** The gateway key, JWT secrets, and connector tokens are read
> from the environment (or `@smooai/config`) and **never logged**. A connector
> config stores a secret *name* (`auth_ref`), never the token — see [[Admin API]].

## Server / runtime

| Var | Default | Purpose |
| --- | --- | --- |
| `SMOOTH_AGENT_BIND` | `127.0.0.1` | Bind address. Set `0.0.0.0` in k8s/containers so the Service/Ingress can reach the pod. |
| `SMOOTH_AGENT_PORT` | `8787` | TCP port (the WS endpoint is `ws://host:port/ws`). |
| `SMOOTH_AGENT_MODEL` | `claude-haiku-4-5` | Model id requested from the gateway. |
| `SMOOTH_AGENT_SEED_KB` | *(unset)* | `1` seeds a couple of distinctive demo docs on startup. |
| `SMOOTH_AGENT_MAX_ITERATIONS` | `6` | Agent-loop iteration cap per turn. |
| `SMOOTH_AGENT_MAX_TOKENS` | `512` | `max_tokens` sent to the gateway (kept low — paid endpoint). |
| `RUST_LOG` | `info,smooth_operator=info` | Log verbosity (independent of OTLP export). |

## LLM gateway (and embeddings / rerank)

| Var | Default | Purpose |
| --- | --- | --- |
| `SMOOAI_GATEWAY_URL` | `https://llm.smoo.ai/v1` | OpenAI-compatible gateway base URL (chat + `/v1/embeddings` + `/v1/rerank`). |
| `SMOOAI_GATEWAY_KEY` | *(unset)* | Gateway API key. **When unset:** `send_message` returns a clean `LLM_UNAVAILABLE`; embeddings fall back to the network-free `DeterministicEmbedder` (1024-d); everything else still works. **When set:** the semantic `GatewayEmbedder` (1536-d) is selected. See [[Knowledge and RAG]]. |
| `SMOOTH_AGENT_RERANK` | *(unset ⇒ off)* | `gateway` (cross-encoder over `/v1/rerank`) \| `lexical` (offline) \| unset (off). See [[Reranking]]. |

## Storage

| Var | Default | Purpose |
| --- | --- | --- |
| `SMOOTH_AGENT_STORAGE` | `memory` | `memory` \| `postgres` \| `dynamodb` — selects the [[Storage Adapters|StorageAdapter]] (and the durable [[Admin API|admin stores]]). |
| `SMOOTH_AGENT_DATABASE_URL` / `DATABASE_URL` | *(unset)* | Postgres connection string (for `SMOOTH_AGENT_STORAGE=postgres`). |

(DynamoDB / S3 Vectors use the standard AWS SDK credential + region resolution;
table and bucket names are wired by the deploy — see [[Deploy Architecture]].)

## Authentication

Full model + the JWT contract + the `trusted` proxied mode: [[Access Control]] and
[[Authentication and RBAC]].

| Var | Default | Purpose |
| --- | --- | --- |
| `AUTH_MODE` | `jwt` | `jwt` (BYO IdP) \| `smoo` (hosted Smoo identity) \| `trusted` (proxied — upstream forwards identity, no verification) \| `none` (**dev only**). |
| `AUTH_JWT_HS256_SECRET` | — | HS256 shared secret (for `jwt`/`smoo`). |
| `AUTH_JWT_RS256_PUBLIC_KEY` | — | RS256 PEM public key (takes precedence over HS256). |
| `AUTH_JWT_ISSUER` | — | Required `iss` when set (**required** for `smoo`). |
| `AUTH_JWT_AUDIENCE` | — | Required `aud` when set. |
| `AUTH_DEV_ORG_ID` | `dev-org` | Org id for the `none`-mode admin principal. |

> **Secure-by-default.** `jwt`/`smoo` with **no key** is a hard
> `AuthError::Misconfigured` — the server **refuses to start** rather than falling
> back to no-auth. `none` and `trusted` are reachable only by explicit opt-in;
> `trusted` logs a loud startup warning that identity is trusted without
> verification (only safe behind a trusted proxy). See [[Integrating into an Existing App]].

## Observability

| Var | Default | Purpose |
| --- | --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | *(unset)* | When set, ships `gen_ai.*` spans over OTLP/**gRPC** (default collector port `4317`); unset = local `fmt` logging only, no collector needed. See [[Observability]]. |

## Connector / dev-support extras

| Var | Purpose |
| --- | --- |
| `GITHUB_TOKEN` | GitHub PAT for the [[Connectors|GitHub connector]] / `github_search` (read scope). |
| `SMOOTH_AGENT_E2E` | `1` opts into the gated live LLM / connector tests (skip cleanly otherwise). See [[Evals]]. |
| `SMOOTH_AGENT_JUDGE_MODEL` | Overrides the [[Evals|LLM-judge]] model (judge only; agent stays on `SMOOTH_AGENT_MODEL`). |

## Related

- [[Self-Hosting]] · [[Getting Started]] — where these get set.
- [[Access Control]] — the auth modes in depth.
- [[Deploy Architecture]] — what the SST / Helm deploy wires.
