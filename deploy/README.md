# `deploy/` — deployment targets

All three paths are first-class. The storage adapter (and the in-memory/Redis/NATS backplane + auth seams) is what makes one codebase deploy to any of them.

- `local/` — **local / embed-in-process** (laptop dev + the smooth daemon). Everything in-memory, auth off, no external services. One command: `cargo run -p smooai-smooth-operator-server`, or embed via `serve_local`.
- `sst/` — **AWS serverless** (default, cloud-codable). API Gateway WebSocket + Lambda handlers + DynamoDB (ElectroDB) + S3 Vectors + S3 blobs. One command: `npx smooth-operator deploy`.
- `k8s/` — **Kubernetes / self-host**. Helm chart: service + Postgres + pgvector + ingress. One command: `helm install smooth-operator ./deploy/k8s`.

## Trying the k8s flavor locally

`helm install` assumes a cluster and a Postgres you may not have yet, which made
the primary self-host target the one path you could not try before committing to
it. This brings the whole thing up on a throwaway [kind](https://kind.sigs.k8s.io)
cluster and holds it open:

```bash
# kind + kubectl + helm + docker, and a WS client (websocat, python3 'websockets', or node 'ws')
SMOOAI_GATEWAY_KEY=<your llm.smoo.ai key> KEEP_RUNNING=1 deploy/scripts/kind-smoke.sh
```

It creates the cluster, builds and loads the image, deploys a throwaway pgvector
Postgres, installs **the same chart a self-hoster installs**, runs the protocol
smoke, then holds a port-forward at `ws://127.0.0.1:18787/ws` and prints how to
point [`examples/web-chat`](../examples/web-chat) at it — the same UI the local
flavor uses, so the only thing that changes is what is serving.

Without `SMOOAI_GATEWAY_KEY` the stack still comes up and the UI still connects;
`send_message` returns a clean error instead of a reply. `SKIP_BUILD=1` makes
reruns fast, and Ctrl-C leaves the cluster up (`kind delete cluster --name
smooth-agent-smoke` removes it).

The same script without `KEEP_RUNNING` is the CI gate
(`.github/workflows/pr-kind-deploy-smoke.yml`), so the local example and the
deployment test cannot drift apart — there is only one of them.

See [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) §6 for the target matrix.

## Container images

Every server implementation ships a Dockerfile. All build from the **repo root**
as context, run non-root as uid 10001, and default their bind to `0.0.0.0` —
the process defaults stay on loopback, which is correct outside a container and
unreachable inside one.

| Server     | Dockerfile                      | Port | Bind env                                  | Alias still honored                       |
| ---------- | ------------------------------- | ---- | ----------------------------------------- | ----------------------------------------- |
| Rust       | `Dockerfile`                    | 8787 | `SMOOTH_AGENT_BIND` + `SMOOTH_AGENT_PORT` | —                                         |
| .NET       | `dotnet/server/host/Dockerfile` | 8787 | `SMOOTH_AGENT_BIND` + `SMOOTH_AGENT_PORT` | `ASPNETCORE_URLS`                         |
| Go         | `go/server/Dockerfile`          | 8787 | `SMOOTH_AGENT_BIND` + `SMOOTH_AGENT_PORT` | `SMOOTH_OPERATOR_BIND` (`host:port`)      |
| Python     | `python/server/Dockerfile`      | 8787 | `SMOOTH_AGENT_BIND` + `SMOOTH_AGENT_PORT` | `SMOOTH_OPERATOR_BIND` (`host:port`)      |
| TypeScript | `typescript/server/Dockerfile`  | 8787 | `SMOOTH_AGENT_BIND` + `SMOOTH_AGENT_PORT` | `SMOOTH_OPERATOR_HOST` + `_PORT`          |

Every implementation reads the same `SMOOTH_AGENT_*` names and defaults to `8787`,
so the same `docker run` works against any of them and switching engines is a
one-word change. Each server's PRE-PARITY name is kept as an alias — the canonical
name wins when both are set — so no existing deployment breaks. The full alias
table is in [Configuration](../docs/Reference/Configuration.md).

```bash
docker build -f go/server/Dockerfile -t smooth-operator-server-go .
docker run --rm -p 8787:8787 -e SMOOAI_GATEWAY_KEY=sk-... smooth-operator-server-go
```

The Go/Python/TypeScript images confine the agent's coding tools to `/workspace`
(mount a project there with `-v "$PWD:/workspace"`); without a dedicated cwd the
tools would default to `/` and scope to the whole container filesystem.
