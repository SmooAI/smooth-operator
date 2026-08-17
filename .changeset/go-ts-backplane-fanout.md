---
"@smooai/smooth-operator": patch
---

feat(go,ts): full 5-target backplane fan-out — all five servers now match

Closes the inverted parity this workstream surfaced: Rust, Python and .NET
delivered to `connection` + `session`/`user`/`org`/`agent`, while Go and
TypeScript were connection-only and answered `501 UNSUPPORTED_TARGET` for the
other four. That 501 was honest — a connId→sink registry genuinely cannot route a
session id — but it left the reference ahead of two of its ports.

Both now carry the reference's fan-out, built the same way in each:

- `Target{Kind, ID}` — a comparable struct in Go (a map key by value); an
  interface in TypeScript, keyed internally as `kind\0id` because a colon
  separator would collide on ids that legitimately contain one (an org name, an
  email).
- `Associate(connId, target)` links conn↔target in **both** directions, so
  `Detach` tears every association down rather than leaking one that resolves to
  a closed socket. Idempotent: the session chokepoint runs on every
  sessionId-bearing frame, so a re-association must not double-count.
- `Publish(target, event)` replaces `Publish(connId, event)`; `Attach` seeds
  `("connection", connId)`, so connection delivery is unchanged and needed no
  special case. TypeScript keeps `publish`/`associate` optional on the interface,
  so a third-party backplane predating them still gets the honest 501 — that one
  really cannot route.

The lifecycle wiring is the load-bearing half: `user`/`org` at connect from the
**authenticated principal** — never a frame field — and `session`/`agent` as
sessions resolve.

**TypeScript needed a chokepoint it did not have.** Go, Python, Rust and .NET each
funnel every client-supplied sessionId through one guard (`scopedSession` /
`_visible_session` / `ScopedSessionAsync`); TypeScript re-derived the ownership
check at three call sites. All three were byte-identical, so they now route
through a new `scopedSession`. That is where association lives, for the same
reason the ownership check belongs there: one place covers every handler. Worth
noting on its own — a missing funnel is exactly the shape of th-1b7ed0.

`delivered` stays truthful in both. A `session` target with nothing associated now
returns a real `{"delivered": 0}`, because the type IS routable — 501 would be the
lie now. It was correct only while the registry could not resolve it.

11 new tests, and each server's 501 test is rewritten to assert real delivery for
all four kinds. The registry tests port Rust's `backplane.rs`, including the
idempotent-associate case that the hot chokepoint path makes matter.

Delivery coverage is now identical across all five servers:

| Server | connection | session / user / org / agent |
| --- | --- | --- |
| Rust | yes | yes |
| Python | yes | yes |
| .NET | yes | yes |
| Go | yes | yes |
| TypeScript | yes | yes |
