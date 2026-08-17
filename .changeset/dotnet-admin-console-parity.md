---
"@smooai/smooth-operator": patch
---

feat(dotnet): serve the full `/admin/*` management API, so the console renders against .NET

The C# host shipped four admin routes — `/admin/health`, `/admin/me`, a repo-listing
`/admin/connectors`, and its own `POST /admin/reindex`. The console needs sixteen, so
four of its five pages 404'd and .NET was the one engine the management UI could not
drive. This adds the ten missing route families (eleven method+path combos): the
conversations list and per-conversation messages, indexing runs, document sets, the
full connector CRUD plus `POST /admin/connectors/{id}/index`, and settings GET/PUT.

Two of the existing routes changed shape to match the contract, because the .NET-only
shapes could never have rendered. `/admin/me` now answers `{userId, orgId, role}`
instead of `{sub, org, role, groups}`, and `/admin/connectors` now lists persisted,
org-scoped connector configs rather than the env-configured GitHub repos — those are
still reachable through `POST /admin/reindex`, which is unchanged and remains this
host's own extra route. Shapes are built against `console/lib/types.ts`, not the Rust
struct field names: those read snake_case in source but serialize camelCase, so
copying them yields a server that passes its own tests and renders nothing.

The auth gate is the one the other four servers use, and it fails closed in both
directions. A missing bearer token is 401 even on a no-auth server; a token an
auth-enabled server cannot verify is 401 rather than an anonymous grant; below the
required role is 403. `AUTH_MODE=none` resolves to an **Admin** principal, matching
Rust's `NoAuthVerifier` — without that the console 403-walls against a local server,
which is exactly as useless as the 404s this closes. The gate also now resolves
through the same `IAuthVerifier` seam the WebSocket host uses, so a host running with
`SMOOTH_LOCAL_TOKEN` authenticates admin requests instead of silently falling back to
the `TokenAccessResolver`.

Org scoping lives in the handlers, not the store: a cross-org id 404s identically to
an unknown one, so the API is never an existence oracle for another org's rows, and
the internal owner key is `[JsonIgnore]`d off every response. `GET /admin/settings`
returns defaults on a miss rather than 404.

Backed by an in-memory `AdminStores` for now, as in the Go and TypeScript servers; a
host can register its own in DI to swap in durable storage without touching the
handlers. Document sets are honestly empty and a connector index run records zero
documents — this server has no per-connector ingestion pipeline yet, and inventing
counts would render a lie.

Verified by 28 xUnit integration tests over real HTTP against a booted host
(auth-fail-closed on every gated route, role rank in both directions, the no-auth
admin grant and that it does not leak into an auth-enabled server, org isolation, and
each response shape), and by booting the Next.js console with `CONSOLE_AUTH=dev`
against the C# host: all five pages render, and the connector create → edit → index →
delete and settings-save flows round-trip through the UI, with zero `/admin/*` 404s.
