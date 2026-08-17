# syntax=docker/dockerfile:1
#
# Multi-stage image for the Go smooth-operator server (local flavor).
# Build from the REPO ROOT — go/server has a `replace` onto the sibling client
# module at go/, so both must be in the context:
#
#     docker build -f go/server/Dockerfile -t smooth-operator-server-go .

# ── Builder ────────────────────────────────────────────────────────────────
# go/server/go.mod declares `go 1.26`; the toolchain must be at least that.
FROM golang:1.26-bookworm AS builder

WORKDIR /src

# Manifests first so the module download layer caches independently of source.
COPY go/go.mod go/go.sum ./go/
COPY go/server/go.mod go/server/go.sum ./go/server/

WORKDIR /src/go/server
RUN --mount=type=cache,target=/go/pkg/mod go mod download

WORKDIR /src
COPY go/ ./go/

# CGO off so the binary runs on the slim runtime without a libc toolchain.
WORKDIR /src/go/server
RUN --mount=type=cache,target=/go/pkg/mod \
    --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 go build -o /smooth-operator-server ./cmd/serve

# ── Runtime ────────────────────────────────────────────────────────────────
# Matches the Rust image: slim Debian + ca-certificates so the server can reach
# the HTTPS LLM gateway (https://llm.smoo.ai/v1).
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user (same uid/gid as the Rust image).
RUN groupadd --system --gid 10001 smooth \
    && useradd --system --uid 10001 --gid smooth --no-create-home --shell /usr/sbin/nologin smooth

COPY --from=builder /smooth-operator-server /usr/local/bin/smooth-operator-server

# The coding tools are confined to SMOOTH_WORKSPACE, which defaults to the
# process cwd. Give that a dedicated writable directory: without it cwd is `/`
# and the agent's file tools would be scoped to the whole container filesystem
# (its own binary, /etc) rather than to data. Mount your project over it.
RUN install -d -o 10001 -g 10001 /workspace
WORKDIR /workspace

USER 10001:10001

# Bind + port (both overridable), the canonical names every server implementation
# reads (see go/server/cmd/serve/main.go). The PROCESS default is 127.0.0.1, which
# is unreachable from outside a container — so the IMAGE defaults the host to
# 0.0.0.0. Narrow it back with -e SMOOTH_AGENT_BIND=127.0.0.1 if you front it with
# a sidecar/proxy on the pod loopback. The legacy combined SMOOTH_OPERATOR_BIND is
# still honored for existing deployments.
ENV SMOOTH_AGENT_BIND=0.0.0.0 \
    SMOOTH_AGENT_PORT=8787
EXPOSE 8787

ENTRYPOINT ["/usr/local/bin/smooth-operator-server"]
