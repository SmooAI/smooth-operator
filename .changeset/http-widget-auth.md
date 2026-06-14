---
'@smooai/smooth-operator': minor
---

HTTP-backed widget auth (SMOODEV-1890): `HttpWidgetAuth`, a generic `WidgetAuthProvider` that resolves each agent's embed policy (`allowed_origins` + `public_key`) by GETting `{base_url}/{agentId}` from a host policy service, with TTL caching. Response handling fails safe: 2xx caches the policy, 404 caches a no-policy result (denied under `WIDGET_AUTH_STRICT`), and 5xx/network/malformed responses return `None` without caching so the next connect retries. The server now installs it from env — set `WIDGET_AUTH_URL` (plus optional `WIDGET_AUTH_BEARER` / `WIDGET_AUTH_TTL_SECS`) to enforce embeddable-widget auth against a host's policy service with no custom binary; unset leaves the permissive default. This is the reusable mechanism a host backs with its own agent store (SmooAI points it at an api-prime route).
