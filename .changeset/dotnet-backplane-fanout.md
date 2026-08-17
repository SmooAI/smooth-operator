---
"@smooai/smooth-operator": patch
---

feat(dotnet): associate session/user/org/agent so the backplane actually fans out

core#414 and core#418 built .NET's backplane up to a target-shaped
`Publish(Target, event)` with a `target → connections` index, and left the last
step explicit: only `Target("connection", …)` had entries, so the other four
kinds resolved to zero connections and `POST /admin/publish` 501'd them. This is
that step — the associations — so all five targets deliver for real.

Additive, no reshaping: `Associate(connectionId, target)` moves onto `IBackplane`
(it existed as a private helper), and `Attach` keeps seeding the connection
target, so connection delivery is unchanged and needed no special case.

The lifecycle wiring is what makes it more than an index. `Attach`/`Detach` were
already in `PumpAsync`; this adds `user`/`org` at connect from the
**authenticated principal** — never a frame field — and `session`/`agent` as
sessions resolve. That hook goes on `FrameDispatcher.ScopedSessionAsync`, already
the security chokepoint every sessionId-bearing action routes through, plus
session creation, so no handler can work with a session the backplane does not
know about. `Associate` is idempotent because that chokepoint runs on every
sessionId-bearing frame.

`delivered` stays truthful, and the 501s go away rather than being papered over:
a `session` target with nothing associated now returns a real
`{"delivered": 0}` — the type IS routable, so 501 would be the lie now. It was
correct only while a connection-id registry could not resolve it.

One hazard the fan-out introduces and this fixes at the source: `Publish` handed
the SAME `JsonObject` to every sink. That was fine at one sink per target; with
many it lets one connection's sink corrupt every other connection's frame, since
`JsonObject` is mutable. Each sink now gets its own `DeepClone`, which also makes
the route's pre-publish clone redundant, so it's gone.

7 new tests, and core#414's 501 theory is rewritten to assert real delivery for
all four kinds. The one that earns its keep drives a **real WebSocket** through
`create_conversation_session`, publishes to that connection's
session/user/org/agent, and asserts the events **land on the socket** — not that a
counter moved — then that after close the session is unroutable again. That is the
test that fails if the association wiring silently never runs.

Delivery coverage is now Rust, Python and .NET on full fan-out; Go and TypeScript
remain connection-only with an honest 501, and are next.
