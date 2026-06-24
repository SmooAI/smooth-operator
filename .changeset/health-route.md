---
"@smooai/smooth-operator": patch
---

Add an unauthenticated `GET /health` HTTP route to `smooth-operator-server`. A WebSocket `/ws` upgrade can't answer a plain GET healthcheck, so HTTP load balancers (AWS ALB, nginx ingress) had nothing to probe; `GET /health` now returns `200 OK`, dependency-free (no storage/LLM touch). Enables HTTP health checks for the K8s deployment flavor.
