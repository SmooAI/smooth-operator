# syntax=docker/dockerfile:1
#
# Multi-stage image for the TypeScript smooth-operator server (local flavor).
# Build from the REPO ROOT — typescript/server is a pnpm workspace member, so
# the root lockfile and workspace manifest must be in the context:
#
#     docker build -f typescript/server/Dockerfile -t smooth-operator-server-ts .

# ── Builder ────────────────────────────────────────────────────────────────
FROM node:22-bookworm-slim AS builder

# Match the repo's packageManager pin (root package.json).
RUN corepack enable && corepack prepare pnpm@9.15.0 --activate

WORKDIR /src

# Workspace manifests first so the install layer caches independently of source.
# Every member's package.json is needed for pnpm to resolve the lockfile, even
# though only the server is installed below.
COPY pnpm-workspace.yaml pnpm-lock.yaml package.json ./
COPY typescript/package.json ./typescript/
COPY typescript/server/package.json ./typescript/server/
COPY typescript/extension-sdk/package.json ./typescript/extension-sdk/
COPY typescript/create-smooth-extension/package.json ./typescript/create-smooth-extension/
COPY console/package.json ./console/
COPY examples/web-chat/package.json ./examples/web-chat/

# Only the server's subtree — console/ and the examples are not in this image.
RUN --mount=type=cache,target=/pnpm-store \
    pnpm config set store-dir /pnpm-store \
    && pnpm install --frozen-lockfile --filter @smooai/smooth-operator-server...

COPY typescript/server/ ./typescript/server/

WORKDIR /src/typescript/server
RUN pnpm build

# Prune to production deps only; the result is self-contained under /out.
RUN --mount=type=cache,target=/pnpm-store \
    pnpm --filter @smooai/smooth-operator-server --prod deploy /out

# ── Runtime ────────────────────────────────────────────────────────────────
FROM node:22-bookworm-slim AS runtime

# ca-certificates so the server can reach the HTTPS LLM gateway
# (https://llm.smoo.ai/v1). The node base ships them, but pin the intent.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user (same uid/gid as the Rust image). The node image ships
# its own `node` user at uid 1000; we use 10001 so all five images match.
RUN groupadd --system --gid 10001 smooth \
    && useradd --system --uid 10001 --gid smooth --no-create-home --shell /usr/sbin/nologin smooth

WORKDIR /app
COPY --from=builder /out /app

# The coding tools are confined to SMOOTH_WORKSPACE, which defaults to the
# process cwd. Give that a dedicated writable directory: without it cwd is `/`
# and the agent's file tools would be scoped to the whole container filesystem
# (its own binary, /etc) rather than to data. Mount your project over it.
RUN install -d -o 10001 -g 10001 /workspace
WORKDIR /workspace

USER 10001:10001

# Unlike the Go/Python servers, the TS server reads host and port as SEPARATE
# vars (see typescript/server/src/main.ts). The PROCESS default host is
# 127.0.0.1, which is unreachable from outside a container — so the IMAGE
# defaults it to 0.0.0.0, keeping the port on its 8787 default. Narrow it back
# with -e SMOOTH_OPERATOR_HOST=127.0.0.1 if you front it with a sidecar/proxy on
# the pod loopback.
ENV SMOOTH_OPERATOR_HOST=0.0.0.0 \
    SMOOTH_OPERATOR_PORT=8787
EXPOSE 8787

ENTRYPOINT ["node", "/app/dist/main.js"]
