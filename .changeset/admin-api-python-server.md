---
"@smooai/smooth-operator": patch
---

feat(python): the `/admin/*` management API, so the console works against the Python server

The management console 404s against the Python server: only Rust and C#
implemented the admin API. The Python server now serves the same 14 endpoints the
console's typed client calls, on the same wire contract as
`rust/smooth-operator-server/src/admin.rs` — same paths, camelCase JSON, and the
`{"error":{"code","message"}}` envelope.

Auth matches Rust: Bearer token → verify → role-rank gate, 401 for a missing or
invalid token and 403 for an insufficient role, `/admin/health` ungated.
`AUTH_MODE=none` grants Admin exactly as Rust's `NoAuthVerifier` does, so the
console doesn't 403-wall against a local server; an auth-enabled server is
unaffected.

**One deviation, forced by the transport.** Rust, Go, C# and TypeScript serve
`/admin/*` and `/ws` on one port. `websockets`' handshake parser accepts GET only
and raises on any non-zero `Content-Length`, so its `process_request` hook cannot
serve a POST/PUT API — the request never reaches it. The admin API therefore
listens on its own port (default: ws port + 1, override with `admin_port`, read
back via `Server.admin_port`). The console configures its admin base URL
separately from the WS URL, so this is a config value, not a code change.
