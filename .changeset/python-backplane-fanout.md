---
"@smooai/smooth-operator": patch
---

feat(python): real backplane fan-out + `POST /admin/publish` (all five targets)

The Python server could not deliver a realtime event to a connected client at
all. Its backplane held connection-id **strings** in a set — no sink, so nothing
to deliver *to*, and no `associate`/`publish` at all — and it had no
`/admin/publish` route. Non-AI publishers (job status, ingestion progress,
notifications) had no way to reach a socket without going through an agent turn.

Ports the Rust reference's `rust/smooth-operator/src/backplane.rs` in full,
including its **5-target fan-out**:

- `attach(conn_id, sink)` registers the connection's outbound sink; re-attach
  replaces it, so a reconnect under the same id never leaves a dead socket
  receiving. `Target("connection", conn_id)` is always reachable.
- `associate(conn_id, target)` records a conn↔target link in both directions, so
  `detach` can tear all of them down.
- `publish(target, event)` delivers to every connection for that target and
  returns the count of **local** deliveries.

Wired into the real connection lifecycle, which is the part that makes it more
than a registry: the sink is attached after it exists (a connection registered
without one would report a delivery it cannot make), `user`/`org` are associated
at connect from the authenticated principal, and `session`/`agent` are associated
as sessions resolve. That hook sits in `_visible_session` — already the single
funnel every sessionId-bearing action goes through — plus session creation, so no
handler can work with a session the backplane does not know about.

`POST /admin/publish` is new and Admin-gated, matching Rust's wire contract:
`{"target": {"type", "id"}, "event": {...}}` → `{"delivered": n}`. **Unlike the
Go and TypeScript servers, which route by connection id only and honestly 501 the
other four kinds, all five targets are deliverable here** — this backplane has
the fan-out to back them.

`delivered` stays truthful: it counts this process's sockets, so `0` means
"nobody on this pod", never a fabricated success. A publish to an unknown target
returns `{"delivered": 0}` rather than pretending, and a bad body is a 400 rather
than a silent no-op.

One deliberate design note: `publish` snapshots the matching sinks under the
registry lock and calls them **outside** it. The sinks are non-blocking enqueues,
but a host's sink is arbitrary code and invoking it under the lock would let one
bad sink deadlock every connection.

11 new tests. The registry tests port Rust's `backplane.rs` unit tests; the route
tests port its `tests/admin_publish.rs`; and one end-to-end test drives a real
connection loop through `create_conversation_session`, then publishes to its
session/user/org/agent and asserts the events **land on the socket** — not just
that a counter incremented — and that detach makes it unroutable again. That last
one is the test that would catch the association wiring silently never running.

.NET is untouched: it has no backplane, no connection registry and no
`/admin/publish`, so it needs its own build-out plus coordination with the .NET
admin work rather than being folded in here.
