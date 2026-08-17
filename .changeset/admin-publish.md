---
"@smooai/smooth-operator": patch
---

feat(go,ts): `POST /admin/publish`, with an honest `delivered` count

The second Rust-only admin route (epic th-9e792d item R). Admin-gated; pushes a
realtime event to a connected client without going through an agent turn — the
plug point for non-AI publishers (job status, ingestion progress, notifications).

**`delivered` never lies.** These servers' backplanes are a connection-id → sink
registry, so only `connection` targets are routable. Rust additionally fans out
to `session` / `user` / `org` / `agent` over a richer backplane; here those
answer **501 `UNSUPPORTED_TARGET`** and carry no `delivered` field at all, rather
than a misleading `{"delivered": 0}` that a caller would read as "accepted,
reached nobody" for an event that was never routable. A genuine 0 — a routable
`connection` target that simply isn't attached — is still reported as 0.

To make the count truthful, `Backplane.Publish` now returns the number of sinks
reached (Go: signature change, no existing callers; TypeScript: a new **optional**
`publish` method, so a third-party backplane that predates it stays valid and the
route answers 501 when it's absent).

Python and .NET are deliberately excluded: their backplanes hold no sink to
deliver to, which is connection-lifecycle work rather than an admin-route port.
