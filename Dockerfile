# syntax=docker/dockerfile:1
#
# Multi-stage image for the smooth-operator WebSocket server.
#
# ──────────────────────────────────────────────────────────────────────────
#  SINGLE-REPO BUILD CONTEXT
# ──────────────────────────────────────────────────────────────────────────
# The Rust workspace depends on `smooai-smooth-operator-core` from crates.io (a
# plain `version` dep — see rust/Cargo.toml), so the build context is THIS repo
# alone. cargo fetches the engine crate from the registry during the build.
#
#     docker build -f Dockerfile -t smooth-operator:dev .
#
# (Previously the context had to span a sibling smooth-operator-core checkout for
# a path dep; that's gone now the crate is published.)
# ──────────────────────────────────────────────────────────────────────────

# ── Builder ────────────────────────────────────────────────────────────────
# Pin a Debian-bookworm Rust toolchain. The workspace is edition 2021; any
# recent stable (1.74+) satisfies axum 0.8 / tokio 1. `rust:1-bookworm` tracks
# the latest stable 1.x on bookworm so the runtime glibc matches the
# `debian:bookworm-slim` final stage.
FROM rust:1-bookworm AS builder

# Build deps for the postgres adapter / TLS-capable crates (openssl, pkg-config).
# Kept minimal; the server bin itself is pure-Rust + axum but the workspace
# pulls the postgres adapter into the build graph.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Single-repo context: copy this repo; the engine crate comes from crates.io.
COPY . /src/smooth-operator/

WORKDIR /src/smooth-operator/rust

# Build only the server binary in release mode.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --locked -p smooai-smooth-operator-server \
    && cp target/release/smooth-operator-server /smooth-operator-server

# ── Runtime ────────────────────────────────────────────────────────────────
# Slim Debian with ca-certificates so the server can reach the HTTPS LLM
# gateway (https://llm.smoo.ai/v1) and a TLS Postgres.
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user.
RUN groupadd --system --gid 10001 smooth \
    && useradd --system --uid 10001 --gid smooth --no-create-home --shell /usr/sbin/nologin smooth

COPY --from=builder /smooth-operator-server /usr/local/bin/smooth-operator-server

USER 10001:10001

# Default WS port (overridable via SMOOTH_AGENT_PORT). Documented in
# rust/.../config.rs. NOTE: the server currently binds 127.0.0.1 — for k8s it
# must bind 0.0.0.0. See deploy/k8s/README.md "0.0.0.0 bind follow-up".
ENV SMOOTH_AGENT_PORT=8787
EXPOSE 8787

ENTRYPOINT ["/usr/local/bin/smooth-operator-server"]
