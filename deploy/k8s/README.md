# smooth-operator-agent — Kubernetes deploy (Helm + ArgoCD)

> 📦 **The canonical chart now lives in [`SmooAI/deploy`](https://github.com/SmooAI/deploy)
> (`helm/smooth-agent`).** That shared chart was extracted from this directory so
> both `smooth-operator-agent` and the `smooai` monorepo can consume one chart.
> This `deploy/k8s/` copy is retained as a **self-contained, deployable mirror**
> (so this repo stays standalone-cloneable), but new chart changes should land in
> `SmooAI/deploy` first and be mirrored back, or this dir should become a thin
> values overlay that depends on the shared chart:
>
> ```yaml
> # deploy/k8s/Chart.yaml (overlay form — depend on the shared chart)
> apiVersion: v2
> name: smooth-operator-agent
> version: 0.1.0
> dependencies:
>   - name: smooth-agent
>     version: 0.1.x
>     repository: file://../../../deploy/helm/smooth-agent   # sibling SmooAI/deploy checkout
>     # or, once published: repository: oci://ghcr.io/smooai/charts
> ```
>
> ```yaml
> # deploy/k8s/values.yaml (overlay form — overrides nested under the subchart name)
> smooth-agent:
>   image: { repository: ghcr.io/smooai/smooth-operator-agent, tag: "0.1.0" }
>   gateway:  { keySecretRef: { name: smooth-operator-agent-gateway, key: SMOOAI_GATEWAY_KEY } }
>   database: { urlSecretRef: { name: smooth-operator-agent-db, key: DATABASE_URL } }
>   ingress:  { enabled: true, className: nginx, host: smooth-operator-agent.smoo.ai }
> ```
>
> then `helm dependency update deploy/k8s && helm install … deploy/k8s`. See
> [`SmooAI/deploy/docs/Consuming.md`](https://github.com/SmooAI/deploy/blob/main/docs/Consuming.md).

The self-host / Kubernetes path for the `smooth-operator-agent` WebSocket
server. This is the `deploy/k8s` half of the dual SST-(AWS)/k8s plan in
[`../../docs/DEPLOY.md`](../../docs/DEPLOY.md): an axum `/ws` service backed by a
**pgvector Postgres** (OLTP + checkpoints + vectors), fronted by an Ingress with
WebSocket-friendly settings, delivered via Helm and synced by ArgoCD.

```
deploy/k8s/
├── Chart.yaml
├── values.yaml
├── templates/
│   ├── _helpers.tpl
│   ├── configmap.yaml      # non-secret env (the SMOOTH_AGENT_* / SMOOAI_GATEWAY_URL contract)
│   ├── secret.yaml         # chart-managed Secret (inline values only; prefer external secrets)
│   ├── deployment.yaml     # the server container; TCP liveness/readiness on the WS port
│   ├── service.yaml        # ClusterIP, port → ws
│   ├── ingress.yaml        # WebSocket annotations + optional TLS
│   ├── hpa.yaml            # optional HPA
│   ├── serviceaccount.yaml
│   └── NOTES.txt
├── argocd/
│   └── application.yaml    # ArgoCD Application (automated sync, prune, selfHeal)
└── README.md
```

---

## 1. Build the image

> ⚠️ **The image build spans TWO repos.** The Rust workspace
> (`rust/Cargo.toml`) has a **path dependency on a sibling repo**:
>
> ```toml
> smooai-smooth-operator = { path = "../../smooth-operator/rust/smooth-operator" }
> ```
>
> Relative to the workspace at `rust/`, that resolves to
> `<repo-parent>/smooth-operator/rust/smooth-operator` — **outside this repo**.
> A Docker context rooted at this repo alone cannot see it, so the build would
> fail at `cargo build`. Until `smooai-smooth-operator` is published to crates.io
> (**roadmap Phase 0**, which deletes the path dep and lets us build from a
> single-repo context), the image **must** be built with a context that spans
> both repos.

Lay the two repos out as siblings (the standard `~/dev/smooai/` layout):

```
<parent>/                       # e.g. ~/dev/smooai
├── smooth-operator-agent/      # this repo
└── smooth-operator/            # the engine (sibling)
```

Then build **from the parent directory**, pointing `-f` at this repo's Dockerfile:

```bash
cd ~/dev/smooai            # the parent that holds BOTH repos
docker build \
  -f smooth-operator-agent/Dockerfile \
  -t ghcr.io/smooai/smooth-operator-agent:0.1.0 \
  .
docker push ghcr.io/smooai/smooth-operator-agent:0.1.0
```

Inside the build the repos land at `/src/smooth-operator-agent` and
`/src/smooth-operator`, preserving the `../../smooth-operator/...` relative path
the workspace expects. The Dockerfile is multi-stage: `rust:1-bookworm` builds
`--release -p smooai-smooth-operator-agent-server`, then a `debian:bookworm-slim`
runtime stage (ca-certificates, non-root uid 10001) copies just the binary.

**Would it build?** Yes — with the cross-repo context above it builds the server
binary cleanly (the workspace path-dep resolves, axum/tokio/postgres-adapter
crates are all on crates.io). It will **not** build from a context rooted at this
repo alone — that's by design, and the Dockerfile header says so rather than
silently failing. We deliberately did **not** run a full `docker build` here (the
cold cross-repo Rust release build is long); the invocation above is the exact
one to use.

`.dockerignore` (repo root) keeps `target/`, `node_modules/`, env files, and
`.git` out of the context.

---

## 2. Postgres / pgvector requirement

The server's Postgres adapter (`rust/adapters/postgres`) reads
`SMOOTH_AGENT_DATABASE_URL` first, then `DATABASE_URL`. The database **must have
the `pgvector` extension available** — the adapter runs
`CREATE EXTENSION IF NOT EXISTS vector;` and creates a `knowledge_vectors` table
with a `vector(N)` column for dense HNSW retrieval (∪ sparse `tsvector` BM25).

A plain Postgres image will fail. Use a pgvector-enabled Postgres:

- `pgvector/pgvector:pg16` (or `ankane/pgvector`) for a self-managed pod,
- CloudNativePG with the `pgvector` extension enabled,
- AWS RDS / Aurora Postgres with the `pgvector` extension installed.

This chart treats Postgres as **external** (`postgres.external: true`) and does
**not** create a Postgres pod — these tables want a long-lived, backed-up DB. To
spin up a throwaway in-cluster pgvector for dev, add a Postgres subchart
dependency (see the commented note in `Chart.yaml`) and point a pgvector image
in its values.

---

## 3. Wire the secrets

Two secrets feed the server: the **gateway key** (`SMOOAI_GATEWAY_KEY`) and the
**database URL**. Each can be supplied two ways:

### Recommended (prod): reference an existing Secret

Create the secrets out-of-band (or via external-secrets-operator), then point the
chart at them — nothing secret lands in your values file or the ArgoCD manifest:

```bash
kubectl create secret generic smooth-operator-agent-gateway \
  --namespace smooai-smooth-operator-agent \
  --from-literal=SMOOAI_GATEWAY_KEY="$GATEWAY_KEY"

kubectl create secret generic smooth-operator-agent-db \
  --namespace smooai-smooth-operator-agent \
  --from-literal=DATABASE_URL="postgresql://user:pass@pg-host:5432/smooth?sslmode=require"
```

```yaml
gateway:
  keySecretRef: { name: smooth-operator-agent-gateway, key: SMOOAI_GATEWAY_KEY }
database:
  urlSecretRef: { name: smooth-operator-agent-db, key: DATABASE_URL }
```

> The deployment maps the DB secret into `SMOOTH_AGENT_DATABASE_URL` (the
> adapter's preferred var). Your secret key can be named anything — set
> `database.urlSecretRef.key` to match.

### Dev only: inline values

`--set gateway.key=sk-...` and `--set database.url=postgres://...` write a
chart-managed `Secret`. Convenient locally; **don't** commit these into a values
file or ArgoCD manifest.

> The gateway key is **optional at startup**. With no key the server still binds
> and answers protocol-only actions (`ping`, `create_conversation_session`);
> `send_message` returns a clean `error` event. Useful for protocol smoke tests
> with zero credentials.

---

## 4. `helm install`

```bash
# Render-check first
helm lint deploy/k8s
helm template smooth-operator-agent deploy/k8s

# Install (external secrets from step 3)
helm upgrade --install smooth-operator-agent deploy/k8s \
  --namespace smooai-smooth-operator-agent --create-namespace \
  --set image.repository=ghcr.io/smooai/smooth-operator-agent \
  --set image.tag=0.1.0 \
  --set gateway.keySecretRef.name=smooth-operator-agent-gateway \
  --set database.urlSecretRef.name=smooth-operator-agent-db \
  --set ingress.enabled=true \
  --set ingress.className=nginx \
  --set ingress.host=smooth-operator-agent.smoo.ai \
  --set ingress.tls.enabled=true
```

Probes are **TCP** on the WS port (`ws`) — a WebSocket upgrade isn't a plain
HTTP GET, so an HTTP probe on `/ws` would 400; a TCP probe just confirms the
listener is up.

### Ingress / WebSocket notes

`ingress.yaml` ships nginx WebSocket annotations
(`proxy-read-timeout`/`proxy-send-timeout: 3600`,
`websocket-services` auto-filled with the Service name). For **AWS ALB** swap
`ingress.annotations` to the ALB set, matching smooai's api-prime ingress, e.g.:

```yaml
ingress:
  className: alb
  annotations:
    alb.ingress.kubernetes.io/scheme: internet-facing
    alb.ingress.kubernetes.io/target-type: ip
    alb.ingress.kubernetes.io/listen-ports: '[{"HTTPS":443}]'
    alb.ingress.kubernetes.io/load-balancer-attributes: idle_timeout.timeout_seconds=3600
    cert-manager.io/cluster-issuer: letsencrypt-prod   # for TLS via cert-manager
```

---

## 4b. Ephemeral-cluster smoke test (kind)

`deploy/scripts/kind-smoke.sh` is a one-shot **deployment** smoke: it stands up
this chart on a throwaway `kind` cluster (backed by a disposable
`pgvector/pgvector:pg16` Postgres), then drives the wire protocol over a **live
WebSocket** to the deployed pod — `ping`→`pong` and
`create_conversation_session`→a valid session id (neither runs an LLM turn, so
**no gateway key is needed**). It replicates the in-process
`rust/.../tests/protocol_smoke.rs` checks across the network/Helm/container
boundary.

```bash
# Full run: create cluster, build the image (cross-repo context), load, deploy, smoke, teardown.
deploy/scripts/kind-smoke.sh

# Fast local reruns against a cluster you keep up:
KEEP_CLUSTER=1            deploy/scripts/kind-smoke.sh   # first run, leave cluster up
SKIP_BUILD=1 KEEP_CLUSTER=1 deploy/scripts/kind-smoke.sh # reuse loaded image, skip docker build
USE_EXISTING_CLUSTER=1   deploy/scripts/kind-smoke.sh   # target your current kube-context
```

It builds with the **cross-repo Docker context** (the parent dir holding both
this repo and the sibling `smooth-operator`; override with `PARENT_DIR`), `kind
load`s the image, `helm install`s with `server.bind=0.0.0.0` and an inline DB
url (no gateway key), `port-forward`s the Service, and runs the protocol smoke
using whichever WS client is available (`websocat`, python `websockets`, or node
`ws`). The cluster is deleted on exit unless `KEEP_CLUSTER=1`.

The CI counterpart is `.github/workflows/pr-kind-deploy-smoke.yml`
(`workflow_dispatch` + PRs touching `deploy/**` / `rust/**` / `Dockerfile`),
which checks out **both** repos as siblings (the cross-repo context), provisions
kind via `helm/kind-action`, and runs the script. That workflow is the **live
gate** — the script is also statically verified (`shellcheck`, `bash -n`).

---

## 5. ArgoCD

`argocd/application.yaml` is an ArgoCD `Application` pointing at this chart
(`repoURL: https://github.com/SmooAI/smooth-operator-agent`, `path: deploy/k8s`,
`targetRevision: main`) with automated sync (`prune: true`, `selfHeal: true`),
`CreateNamespace=true`, a sync-wave annotation, and a retry/backoff — mirroring
the smooai api-prime / ArgoCD pattern. Its `helm.valuesObject` references the
external secrets from step 3, so no credentials live in the manifest.

```bash
kubectl apply -n argocd -f deploy/k8s/argocd/application.yaml
# pin an immutable image.tag in the manifest's valuesObject (or let
# ArgoCD Image Updater manage it via argocd-image-updater.argoproj.io/* annotations).
```

---

## 6. ⚠️ Required follow-up: bind `0.0.0.0`

**The server currently binds `127.0.0.1` and is unreachable from inside the
cluster.** `rust/smooth-operator-agent-server/src/server.rs:78`:

```rust
let addr = SocketAddr::from(([127, 0, 0, 1], config.port));
```

A loopback bind only accepts connections from inside the pod's own network
namespace — the Service / kube-proxy / Ingress all connect over the pod IP, so
every probe and request is refused. **The chart is otherwise complete; it cannot
serve traffic until this one-liner ships.**

The minimal fix (to be applied separately — `rust/` is out of scope for this
chart work) is to bind all interfaces:

```rust
// rust/smooth-operator-agent-server/src/server.rs  (in `bind`)
let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
```

Cleaner, config-driven variant — add a `SMOOTH_AGENT_BIND` (or `SMOOTH_AGENT_HOST`)
env var to `ServerConfig` (default `127.0.0.1` to preserve local-dev behavior,
set to `0.0.0.0` in this chart's ConfigMap):

```rust
// config.rs: add `bind: IpAddr` read from SMOOTH_AGENT_BIND (default 127.0.0.1)
// server.rs: let addr = SocketAddr::from((config.bind, config.port));
```

If you take the env-var route, add `SMOOTH_AGENT_BIND: "0.0.0.0"` to
`templates/configmap.yaml`. Tests bind port 0 on loopback and are unaffected
either way.
