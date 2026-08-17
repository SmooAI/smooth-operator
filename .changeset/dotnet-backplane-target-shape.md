---
"@smooai/smooth-operator": patch
---

refactor(dotnet): make `IBackplane.Publish` target-shaped so the fan-out is additive

`Publish(string connectionId, event)` became `Publish(Target target, event)`, with
`Target(Kind, Id)` as a record. `InMemoryBackplane` now resolves a target to a set of
connections via a `Dictionary<Target, HashSet<string>>`, so `Publish` is **already
correct for all five target kinds** — the other four simply have no entries yet and
return 0. Associating a session/user/org/agent with its connections is the cross-pod
fan-out work, and it plugs in by seeding that index without touching `Publish` again.
`POST /admin/publish` still 501s the four, which keeps that a route-level statement
("not deliverable here") rather than a backplane limitation.

`Detach` now tears down every association, not just the sink, via a reverse index — a
leaked association resolves to a dead socket and would inflate `delivered` forever.

No behavior change: `connection` targets deliver exactly as before, all 518 tests pass
unchanged.
