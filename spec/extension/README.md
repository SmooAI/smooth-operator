# `spec/extension/` — SEP, the Smooth Extension Protocol (source of truth)

Language-neutral JSON Schemas for **SEP**, the protocol smooth-operator engines
speak to long-lived **extension subprocesses**. An extension is any executable
(Node, Python, Rust, …) that talks JSON-RPC 2.0 over its own stdin/stdout; the
engine spawns it, handshakes, and thereafter exchanges tools, hooks, events,
commands and UI requests with it.

SEP is a **sibling** of the operator WebSocket protocol in `spec/` — it reuses
that tree's machinery wholesale (draft 2020-12 JSON Schema, the
`conformance/fixtures.json` fixture pattern, the per-language conformance test)
but **not** its envelope: the operator envelope is asymmetric (client actions /
server events, HTTP-ish status codes for a lossy browser WebSocket), while SEP
is a **symmetric peer RPC** — host-awaits-extension *and* extension-awaits-host —
which is exactly what JSON-RPC 2.0 already is.

- [`envelope.md`](./envelope.md) — framing: JSON-RPC 2.0, ndjson over stdio, the
  method catalog, error codes, the two context tiers, and the deferred WS binding.
- [`methods/`](./methods/) — one `*.schema.json` per method: `params` and (for
  request methods) `result` live under `$defs`.
- [`conformance/fixtures.json`](./conformance/fixtures.json) — valid + invalid
  instances per method. Every engine host replays these against its own serde.
- [`conformance/echo.mjs`](./conformance/echo.mjs) — the dependency-free demo
  extension used as the fixture-replay peer.

`protocolVersion` is an **independent integer** (starts at `1`), decoupled from
engine semver so extensions survive `0.14 → 0.15`-style engine churn. The
handshake negotiates `min(host, ext)`; unknown fields are always ignored;
per-extension load failure is tolerated.
