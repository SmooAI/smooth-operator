# syntax=docker/dockerfile:1
#
# TUI example image. Node 22 + the PUBLISHED @smooai/smooth-operator SDK, run
# interactively. Build context = this directory (no pnpm workspace needed); the
# in-repo `workspace:*` dep is rewritten to a published range before install.
#
#   docker compose run --rm tui   # (compose wires SMOOTH_WS_URL to the operator)
FROM node:22-slim
ARG SMOOTH_SDK_VERSION=^1.41.0
WORKDIR /app
COPY package.json ./
RUN npm pkg set dependencies.@smooai/smooth-operator="${SMOOTH_SDK_VERSION}" \
    && npm install --no-audit --no-fund
COPY . .
ENTRYPOINT ["node", "src/index.mjs"]
