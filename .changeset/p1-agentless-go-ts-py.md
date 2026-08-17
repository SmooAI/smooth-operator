---
"@smooai/smooth-operator": patch
---

fix(go,ts,python): stop inventing an agent id for agentless sessions

Mirrors core#429 (`7b93496`). Session creation filled a missing `agentId` with a
fresh UUID, so every agentless session pointed at an agent that never existed.
Nothing failed loudly — the fabricated id flowed into the backplane agent target
and the per-agent config lookup and quietly resolved to nothing.

`agent_id` is now nullable, the session field optional, and blank/whitespace reads
as absent rather than becoming a literal empty-string agent. Absence is propagated
instead of papered over: the per-agent config resolver isn't called at all for an
agentless session, and `agentId` is omitted from the wire rather than sent as
null/empty.

**Two fabrication sites per server, not one** — the in-memory store and the
Postgres store each had it, so the test covers both. Testing one would have left
the other broken.

Two things the Rust change did that deliberately have **no** counterpart here,
verified rather than mirrored blind:

- The agent participant's `internal_id` — none of these three ever sets it. Their
  participant INSERTs don't name the column, so there was nothing to propagate.
- `agentParticipantId` stays a fresh UUID. That mints a legitimate participant row;
  only `agentId` was the bug.

`Checkpoint.agent_id` is a different type and is untouched.

Go keeps `AgentID string` rather than adding a pointer: `""` is already that struct's
absent, the store already branched on `agentID == ""`, and blank-is-absent is the
specified semantics, so there is no state a pointer would distinguish. The column is
NULL in the database either way, coalesced at the one read site.

One test per language, against real Postgres containers: a session created with a
blank agent id reads back absent from **both** stores, the column is NULL rather than
an empty string standing in for one, and it survives the round trip instead of
returning a uuid.

Green, all exit 0: Go `vet` + `go test`, TypeScript `tsc` + 313 tests, Python ruff +
319 tests.
