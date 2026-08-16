# syntax=docker/dockerfile:1
#
# Web-chat example image: build the Vite/React app on the PUBLISHED
# @smooai/smooth-operator SDK, serve the static bundle with nginx.
#
# Build context = this directory (examples/web-chat) — it does NOT need the pnpm
# workspace. The in-repo package.json pins the SDK as `workspace:*`; for a
# standalone image we rewrite that to a published range before installing.
#
#   docker build -t smooth-operator-web-chat .
#
# The default WS target baked into the bundle is ws://localhost:8787/ws, which is
# correct when the operator's port is published to the host (see docker-compose.yml).
# Point it elsewhere with:  --build-arg VITE_SMOOTH_WS_URL=ws://host/ws

FROM node:22-slim AS build
ARG SMOOTH_SDK_VERSION=^1.41.0
ARG VITE_SMOOTH_WS_URL=ws://localhost:8787/ws
ENV VITE_SMOOTH_WS_URL=${VITE_SMOOTH_WS_URL}
WORKDIR /app
COPY package.json ./
RUN npm pkg set dependencies.@smooai/smooth-operator="${SMOOTH_SDK_VERSION}" \
    && npm install --no-audit --no-fund
COPY . .
RUN npm run build

FROM nginx:alpine AS runtime
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /app/dist /usr/share/nginx/html
EXPOSE 80
