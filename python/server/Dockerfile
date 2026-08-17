# syntax=docker/dockerfile:1
#
# Multi-stage image for the Python smooth-operator server (local flavor).
# Build from the REPO ROOT, for symmetry with the sibling server images:
#
#     docker build -f python/server/Dockerfile -t smooth-operator-server-py .
#
# The project is self-contained — every dependency (including
# smooai-smooth-operator-core) resolves from PyPI, so nothing outside
# python/server/ enters the build.

# ── Builder ────────────────────────────────────────────────────────────────
FROM python:3.12-slim-bookworm AS builder

# uv drives the install; pinned by digest-free tag on the official distroless
# image, copied in rather than curl|sh so the layer is reproducible.
COPY --from=ghcr.io/astral-sh/uv:0.7.8 /uv /usr/local/bin/uv

WORKDIR /app

# Manifests first so the dependency layer caches independently of source.
COPY python/server/pyproject.toml python/server/uv.lock python/server/README.md ./

# --no-install-project: deps only, so editing src doesn't bust this layer.
# --no-dev: pytest/ruff have no business in a runtime image.
RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --locked --no-dev --no-install-project

COPY python/server/src/ ./src/

RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --locked --no-dev

# ── Runtime ────────────────────────────────────────────────────────────────
FROM python:3.12-slim-bookworm AS runtime

# ca-certificates so the server can reach the HTTPS LLM gateway
# (https://llm.smoo.ai/v1). The python base ships them, but pin the intent.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user (same uid/gid as the Rust image).
RUN groupadd --system --gid 10001 smooth \
    && useradd --system --uid 10001 --gid smooth --no-create-home --shell /usr/sbin/nologin smooth

WORKDIR /app
COPY --from=builder /app/.venv /app/.venv
COPY --from=builder /app/src /app/src

# The venv's interpreter, so no `uv` in the runtime image.
ENV PATH="/app/.venv/bin:$PATH" \
    PYTHONPATH=/app/src \
    PYTHONUNBUFFERED=1

# The coding tools are confined to SMOOTH_WORKSPACE, which defaults to the
# process cwd. Give that a dedicated writable directory: without it cwd is `/`
# and the agent's file tools would be scoped to the whole container filesystem
# (its own binary, /etc) rather than to data. Mount your project over it.
RUN install -d -o 10001 -g 10001 /workspace
WORKDIR /workspace

USER 10001:10001

# Bind + port (both overridable), the canonical names every server implementation
# reads (see python/server/src/smooth_operator_server/__main__.py). The PROCESS
# default is 127.0.0.1, which is unreachable from outside a container — so the
# IMAGE defaults the host to 0.0.0.0. Narrow it back with
# -e SMOOTH_AGENT_BIND=127.0.0.1 if you front it with a sidecar/proxy on the pod
# loopback. The legacy combined SMOOTH_OPERATOR_BIND is still honored for existing
# deployments.
ENV SMOOTH_AGENT_BIND=0.0.0.0 \
    SMOOTH_AGENT_PORT=8787
EXPOSE 8787

ENTRYPOINT ["python", "-m", "smooth_operator_server"]
