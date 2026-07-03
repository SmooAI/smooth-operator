---
'@smooai/smooth-operator': minor
---

SEP Phase 7 (spec + SDK + demo) — registerProvider: declarative providers, OAuth,
proxied streaming, and set_model.

**Spec.** New `provider.schema.json` covering `provider/complete` (params +
result), `provider/delta`, and `provider/oauth_login`/`oauth_refresh` (params +
credentials). `initialize`/`registry-update` registrations gain `providers`
(`ProviderRegistration` + `ProviderModel`); `session/set_model` params gain
optional `provider` + `thinking`; `capabilities_enabled` gains `providers`. New
conformance fixtures for every provider shape (valid + `$invalid`), replayed by
both the TypeScript schema conformance test and the Rust host's vendored copy.

**SDK.** `smooth.registerProvider(defineProvider({ name, models, complete,
oauthLogin?, oauthRefresh? }))` — the extension owns the request/stream, emitting
`ctx.delta(event)` chunks while streaming. `session.setModel(model, { provider,
thinking })` completes the Phase 4 session surface. `createTestHost` gains
`complete()` (with `onDelta`), `oauthLogin()`, `oauthRefresh()`, and routes
`provider/delta` by `request_id` — the in-process mirror of the engine's
`ProviderStreams`.

**Demo.** `corporate-proxy` registers a provider that proxies an OpenAI-compatible
endpoint: it streams the upstream SSE back as `provider/delta` chunks, maps
tool-call responses, and mediates OAuth (login prompt over `ui/input`, token
exchange). Exercised end-to-end in `provider-path.test.ts` against a real mock
upstream serving scripted SSE.
