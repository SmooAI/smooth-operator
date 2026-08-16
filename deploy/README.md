# `deploy/` — deployment targets

All three paths are first-class. The storage adapter (and the in-memory/Redis/NATS backplane + auth seams) is what makes one codebase deploy to any of them.

- `local/` — **local / embed-in-process** (laptop dev + the smooth daemon). Everything in-memory, auth off, no external services. One command: `cargo run -p smooai-smooth-operator-server`, or embed via `serve_local`.
- `sst/` — **AWS serverless** (default, cloud-codable). API Gateway WebSocket + Lambda handlers + DynamoDB (ElectroDB) + S3 Vectors + S3 blobs. One command: `npx smooth-operator deploy`.
- `k8s/` — **Kubernetes / self-host**. Helm chart: service + Postgres + pgvector + ingress. One command: `helm install smooth-operator ./deploy/k8s`.

See [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) §6 for the target matrix.

## Container images

Every server implementation ships a Dockerfile. All build from the **repo root**
as context, run non-root as uid 10001, and default their bind to `0.0.0.0` —
the process defaults stay on loopback, which is correct outside a container and
unreachable inside one.

| Server     | Dockerfile                      | Port | Bind env                                    |
| ---------- | ------------------------------- | ---- | ------------------------------------------- |
| Rust       | `Dockerfile`                    | 8787 | `SMOOTH_AGENT_BIND` + `SMOOTH_AGENT_PORT`   |
| .NET       | `dotnet/server/host/Dockerfile` | 8080 | `ASPNETCORE_URLS`                           |
| Go         | `go/server/Dockerfile`          | 8793 | `SMOOTH_OPERATOR_BIND` (`host:port`)        |
| Python     | `python/server/Dockerfile`      | 8787 | `SMOOTH_OPERATOR_BIND` (`host:port`)        |
| TypeScript | `typescript/server/Dockerfile`  | 8787 | `SMOOTH_OPERATOR_HOST` + `SMOOTH_OPERATOR_PORT` |

The bind env differs per implementation because each server's own config surface
does — the images don't invent a shared name, they set whichever one that server
already reads. Go keeps its distinct `8793` default rather than being normalized
to `8787`, so a container matches the process it packages.

```bash
docker build -f go/server/Dockerfile -t smooth-operator-server-go .
docker run --rm -p 8793:8793 -e SMOOAI_GATEWAY_KEY=sk-... smooth-operator-server-go
```

The Go/Python/TypeScript images confine the agent's coding tools to `/workspace`
(mount a project there with `-v "$PWD:/workspace"`); without a dedicated cwd the
tools would default to `/` and scope to the whole container filesystem.
