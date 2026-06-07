# `deploy/sst` — AWS-serverless deploy path

> 📦 **The reusable resources now come from the shared
> [`@smooai/deploy`](https://github.com/SmooAI/deploy) package** (`SmooAI/deploy`,
> `sst/`). This `sst.config.ts` consumes them via the `SmoothAgentApi` construct
> and keeps only the app-specific config (app name/stage/removal + the Lambda
> artifact dir + model). The dependency is wired as a local **path dep** today —
> `"@smooai/deploy": "file:../../../deploy/sst"` (a sibling `SmooAI/deploy`
> checkout in the standard `~/dev/smooai/` layout). The npm-publish follow-up
> (path dep → published `@smooai/deploy`) is tracked in
> [`SmooAI/deploy/docs/Consuming.md`](https://github.com/SmooAI/deploy/blob/main/docs/Consuming.md#publish-follow-up).
>
> ⚠️ pnpm `file:` deps are **content-addressed at install time** — after editing
> the sibling `@smooai/deploy` source, re-run `pnpm install --force` here to pick
> up the change before typechecking.

SST v4 app that consumes the shared `@smooai/deploy` `SmoothAgentApi` construct,
which provisions the AWS-serverless backend for `smooth-operator-agent`:

| Resource | SST component | Notes |
| --- | --- | --- |
| WebSocket transport | `sst.aws.ApiGatewayWebSocket` (`SmoothAgentApi`) | Routes `$connect`, `$disconnect`, `send_message`, `ping`, `$default` → the Rust Lambda |
| Compute | `api.route(...)` Lambda (`provided.al2023`, `arm64`) | One Rust binary; `requestContext.routeKey` dispatches inside it |
| OLTP + checkpoints | `sst.aws.Dynamo` (`SmoothAgentTable`) | Single table: `pk`/`sk` primary + all-projecting `gsi1` GSI, `ttl` enabled. PAY_PER_REQUEST |
| Blobs | `sst.aws.Bucket` (`SmoothAgentBlobs`) | General attachment storage |
| Vectors | S3 Vectors (out-of-band — see below) | Per-org dense-retrieval index |
| Gateway key | `sst.Secret` (`SmoothAgentGatewayKey`) | See "Secrets" below |

The Lambda is the `smooai-smooth-operator-agent-lambda` crate (`rust/smooth-operator-agent-lambda/`). It speaks the schema-driven protocol (`spec/`) over API Gateway WebSocket. Because API Gateway WS invokes the function **once per message** (no persistent socket), the Lambda posts events **back** to the client via the **API Gateway Management API** (`post_to_connection`), and keeps **no** in-process state across invocations — all state lives in DynamoDB + S3 Vectors.

---

## ⚠️ Never deploy locally

This team deploys via **CI**, not from a laptop. A local `sst deploy` can ship unintended state changes and clobber production. Local verification stops at **compile + typecheck + synth** — see "Verify" below.

---

## 1. Build the Rust Lambda bootstrap (required before any deploy)

SST has **no native Rust builder**, so the Lambda bootstrap is built out-of-band with [`cargo lambda`](https://www.cargo-lambda.info/) and SST is pointed at the prebuilt artifact directory.

```bash
# One-time: install cargo-lambda (uses Zig for cross-compilation).
cargo install cargo-lambda

# From the Rust workspace root:
cd ../../rust
cargo lambda build --release --arm64 -p smooai-smooth-operator-agent-lambda
```

This produces:

```
rust/target/lambda/smooai-smooth-operator-agent-lambda/bootstrap
```

…which is exactly the `ARTIFACT_DIR` the SST `Function` `handler` points at (with `runtime: 'provided.al2023'`, `architecture: 'arm64'`). The crate's `[[bin]]` is named `bootstrap` so the artifact matches the `provided.al2023` contract.

> The crate also builds for the host target with a plain `cargo build -p smooai-smooth-operator-agent-lambda` (useful for CI compile checks); only the **deploy** artifact needs the `cargo lambda` cross-build.

## 2. Install SST + generate platform types

```bash
pnpm install          # also links the @smooai/deploy path dep (sibling SmooAI/deploy checkout)
pnpm sst install      # installs providers + generates .sst/platform/config.d.ts (no AWS needed)
```

> The `@smooai/deploy` construct package is a **sibling checkout** at
> `../../../deploy` (clone `SmooAI/deploy` next to this repo). If you edit that
> package, re-run `pnpm install --force` here so pnpm re-copies the `file:` dep.

## 3. Secrets

The smooai monorepo standard is `@smooai/config`. For this **standalone OSS repo** the gateway credentials use `sst.Secret` placeholders instead (documented deviation — adopt `@smooai/config` if/when this is folded back into the monorepo):

```bash
pnpm sst secret set SmoothAgentGatewayKey <gateway-api-key> --stage <stage>
# Optional overrides (have defaults baked into sst.config.ts):
pnpm sst secret set SmoothAgentGatewayUrl https://llm.smoo.ai/v1 --stage <stage>
pnpm sst secret set SmoothAgentModel claude-haiku-4-5 --stage <stage>
```

Without `SmoothAgentGatewayKey` the Lambda still answers protocol-only actions (`ping`, `create_conversation_session`, `get_session`) and returns a clean `LLM_UNAVAILABLE` error for `send_message`.

## 4. Deploy (CI only)

```bash
pnpm sst deploy --stage <stage>     # ⚠️ run from CI, not locally
```

---

## The S3 Vectors gap

Amazon **S3 Vectors** went GA on 2025-12-02. As of SST v4.13 there is **no native SST component** for it, and the AWS Pulumi provider's `s3vectors` resources are new. So this app does **not** create the vector bucket/index as a first-class SST resource by default. Two supported paths:

1. **Raw Pulumi provider (preferred once available):** uncomment the `aws.s3vectors.VectorBucket` / `aws.s3vectors.Index` block in `sst.config.ts`. It declares the bucket `smooth-agent-vectors-<stage>` and a `cosine`/`float32`/`dim=1024` index named `smooth-agent-knowledge-default`.

2. **Out-of-band (works today) — `aws` CLI / CloudFormation:**

    ```bash
    # Vector bucket
    aws s3vectors create-vector-bucket \
      --vector-bucket-name "smooth-agent-vectors-<stage>" --region us-east-1

    # Per-org index (org partition "default"; 1024-dim cosine, matching the adapter)
    aws s3vectors create-index \
      --vector-bucket-name "smooth-agent-vectors-<stage>" \
      --index-name "smooth-agent-knowledge-default" \
      --data-type float32 --dimension 1024 --distance-metric cosine \
      --region us-east-1
    ```

Either way, the Lambda is pointed at the bucket + index prefix via the
`SMOOTH_AGENT_VECTOR_BUCKET` / `SMOOTH_AGENT_VECTOR_INDEX_PREFIX` env vars
(already wired in `sst.config.ts`), and the adapter's `s3-vectors` feature
(enabled in the Lambda crate) does the `PutVectors`/`QueryVectors`. The route's
`permissions` block grants `s3vectors:PutVectors|QueryVectors|GetVectors`.

When `SMOOTH_AGENT_VECTOR_BUCKET` is unset, the adapter transparently falls back
to brute-force cosine over DynamoDB — no S3 Vectors required for dev/lower envs.

---

## Verify (no deploy)

```bash
# 1. Rust Lambda compiles (host target is fine for CI compile checks).
( cd ../../rust && cargo build -p smooai-smooth-operator-agent-lambda )

# 2. SST config typechecks.
pnpm install && pnpm sst install && pnpm typecheck
```

SST v4 has **no creds-free synth** — `sst diff`/`sst deploy` both need AWS state
access, so local verification stops at `tsc --noEmit`. The deploy itself runs in
CI.
