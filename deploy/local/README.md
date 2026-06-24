# `deploy/local` — local deployment flavor (laptop + embed-in-process)

The **third deployment target** alongside [`../sst`](../sst) (AWS serverless) and
[`../k8s`](../k8s) (Kubernetes). Where those two stand up external services —
Postgres / pgvector, NATS or Redis, a `WIDGET_AUTH_URL` policy service, hosted
`AUTH_MODE=smoo` — the **local flavor needs none of them**. It runs the same
`/ws` + `/admin` server with **everything in-memory** and **auth off**, so a
developer can run the operator on a laptop with one command, and a host (e.g. the
smooth daemon) can **embed it in-process** in a few lines of Rust.

There is no chart, no `sst.config.ts`, no infrastructure here — the "deploy" is
just running the binary (or calling [`serve_local`](#embed-in-process)). This
directory is the doc.

---

## One command (no env)

```bash
cd rust
cargo run -p smooai-smooth-operator-server
# → smooth-operator-server listening on ws://127.0.0.1:8787/ws (model=claude-haiku-4-5, llm_enabled=false)
```

With **no environment set**, the binary already boots the local flavor:
`ServerConfig::from_env` defaults to in-memory storage, the in-memory backplane,
loopback bind, and admin disabled. Connect a generated client to
`ws://127.0.0.1:8787/ws` and drive `ping` / `create_conversation_session`
immediately — no credentials required.

`send_message` needs an LLM gateway key; without one it returns a clean protocol
`error` (`LLM_UNAVAILABLE`) instead of hanging. To enable live turns locally,
export the same two gateway vars the other flavors use:

```bash
export SMOOAI_GATEWAY_URL="https://llm.smoo.ai/v1"   # default; override if needed
export SMOOAI_GATEWAY_KEY="sk-…"                      # your gateway key
export SMOOTH_AGENT_SEED_KB=1                         # optional: load the demo knowledge docs
cargo run -p smooai-smooth-operator-server
```

---

## What the local flavor pins

Independent of ambient env, the local flavor always uses:

| Concern | Local flavor | …vs `k8s` / `sst` |
| --- | --- | --- |
| **storage** | in-memory (`InMemoryStorageAdapter`) — lost on restart | Postgres+pgvector / DynamoDB+S3 Vectors |
| **backplane** | in-memory, single-process | NATS / Redis (`SMOOTH_AGENT_BACKPLANE`) for cross-pod fan-out |
| **auth** | none (`NoAuthVerifier`) — `/admin` open, `/ws` boots | `AUTH_MODE=smoo`/`jwt` (`/admin` gated) |
| **widget auth** | permissive (`PermissiveWidgetAuth`) | `WIDGET_AUTH_URL` policy service + `WIDGET_AUTH_STRICT` |
| **bind** | `127.0.0.1:8787` (loopback) | `0.0.0.0` behind a Service/Ingress / API Gateway |

The LLM gateway (`SMOOAI_GATEWAY_URL` / `SMOOAI_GATEWAY_KEY` / `SMOOTH_AGENT_MODEL`)
is read from the environment the same way in every flavor — that's the one piece
the local flavor does **not** pin, so a key in your shell enables real turns.

> ⚠️ **Local flavor is not for production.** In-memory storage is wiped on
> restart and the admin API is unauthenticated. It exists for dev loops and
> in-process embedding, not for serving real traffic — use `deploy/k8s` or
> `deploy/sst` for that.

See [`../../rust/smooth-operator-server/src/config.rs`](../../rust/smooth-operator-server/src/config.rs)
for the full `SMOOTH_AGENT_*` env contract and
[`../README.md`](../README.md) for the target matrix.

---

## Embed in-process

The smooth daemon (and any Rust host) can boot the operator inside its own
process — no child process, no env handshake — via the
[`smooth_operator_server::local`](../../rust/smooth-operator-server/src/local.rs)
module.

### Background server + shutdown handle

```rust
use smooth_operator_server::local::LocalServer;

# async fn demo() -> anyhow::Result<()> {
// In-memory everything, auth off, default loopback `127.0.0.1:8787`.
let server = LocalServer::builder()
    .seed_kb(true)          // optional: load the demo knowledge docs
    // .addr("127.0.0.1:0".parse()?)  // optional: ephemeral port
    .spawn()
    .await?;

println!("local operator on {}", server.ws_url()); // ws://127.0.0.1:8787/ws
// ... connect clients, run turns ...

server.shutdown().await?;    // graceful stop + join the background task
# Ok(())
# }
```

The handle reports the **real** bound address (so `addr("127.0.0.1:0")` →
read `server.addr()` back for the OS-assigned port). Dropping the handle without
calling `shutdown()` signals the server to stop, so a background server never
outlives its owner.

### Run to completion (foreground)

```rust
# async fn demo() -> anyhow::Result<()> {
// Boots the local flavor on the given addr and serves until the process is killed.
smooth_operator_server::local::serve_local("127.0.0.1:8787").await?;
# Ok(())
# }
```

### Customizing the gateway for live turns while embedded

`LocalServer::builder().config(cfg)` takes a full `ServerConfig` (gateway URL /
key / model / limits). The local flavor still **forces** in-memory storage and
the caller's bind addr regardless of what the config says, so the
"no external services" guarantee always holds — `config(..)` only controls the
LLM gateway and turn limits:

```rust
use smooth_operator_server::config::ServerConfig;
use smooth_operator_server::local::{local_config, LocalServer};

# async fn demo() -> anyhow::Result<()> {
let server = LocalServer::builder()
    .config(ServerConfig {
        gateway_key: Some(std::env::var("SMOOAI_GATEWAY_KEY")?),
        ..local_config()   // env-independent defaults with in-memory pinned
    })
    .spawn()
    .await?;
# let _ = server; Ok(())
# }
```

### Lean build (drop the cloud adapters)

By default the server crate compiles **every** backend in (Postgres, DynamoDB,
Redis, NATS) so one binary can serve any flavor — that's the `cloud` feature, and
it's **on by default**, so the `k8s` / `sst` build artifacts are unchanged.

For a laptop dev loop or an **in-process embed**, none of those are needed (the
local flavor is in-memory only), and pulling `tokio-postgres`, the AWS SDK,
`redis`, and `async-nats` just inflates build time and binary size. Opt out with
`--no-default-features` to get a **lean** build that excludes all four cloud
adapters and their transitive deps:

```bash
cd rust
# Lean: in-memory storage + in-memory backplane only — no tokio-postgres / AWS
# SDK / redis / async-nats compiled in.
cargo build -p smooai-smooth-operator-server --no-default-features
cargo run   -p smooai-smooth-operator-server --no-default-features
```

An **embedder** (e.g. the smooth daemon depending on this crate in-process) sets
the same opt-out in its `Cargo.toml` and runs entirely on the in-memory backends:

```toml
[dependencies]
smooai-smooth-operator-server = { version = "1.2", default-features = false }
```

The lean build only supports `SMOOTH_AGENT_STORAGE=memory` and
`SMOOTH_AGENT_BACKPLANE=memory` (the defaults). Requesting a cloud backend at
runtime fails loud with an actionable error (e.g.
`SMOOTH_AGENT_STORAGE=postgres requires building with --features postgres …`)
rather than silently degrading. The gateway-backed semantic embedder/reranker
also live in the postgres adapter crate, so the lean build uses the network-free
`DeterministicEmbedder` / `Noop`/`Lexical` reranker — which is exactly what the
keyless local flavor already does. Re-enable any subset à la carte with
`--features postgres,nats,…` (or the full `--features cloud`).

---

## Verify (no external services)

The local flavor is covered by a hermetic integration test that boots the
embeddable server with in-memory everything and drives `ping` /
`create_conversation_session` over a real WebSocket — no Postgres, Redis, NATS,
AWS, or gateway key:

```bash
cd rust
cargo test -p smooai-smooth-operator-server --test local_flavor
```
