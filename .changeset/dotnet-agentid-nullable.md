---
"@smooai/smooth-operator": patch
---

fix(dotnet): stop inventing an agent id for agentless sessions (th-68897a)

Session creation filled a missing `agentId` with a fresh GUID, so every agentless session
pointed at an agent that never existed. Nothing failed loudly — the fake id flowed into
the participant's `internal_id` and the per-agent config lookup and resolved to nothing.

`StoredSession.AgentId` and `conversation_sessions.agent_id` are now nullable, blank and
whitespace read as absent, and both stores (in-memory and Postgres) stop fabricating.
Minting `AgentParticipantId` is untouched — that mints a real participant row, not a
dangling reference.

The nullable type surfaced the exact path the bug hid in: `IAgentConfigResolver.ResolveAsync`
was being handed the fabricated GUID at two call sites. Both now skip the lookup when there
is no agent, matching the Rust handler.

**A spec conflict this exposes, raised rather than papered over:** the session descriptor is
emitted WITHOUT `agentId` when there is no agent, matching Rust's `skip_serializing_if` — but
`spec/actions/create-conversation-session.schema.json` still lists `agentId` as **required**,
typed `string`. So an agentless descriptor is not spec-valid in *any* language after this
change; null and omission are both invalid. The .NET spec-validity test now names an agent
(the case the spec actually describes) and a separate test pins the agentless shape.

Two existing tests were asserting the bug and are inverted: one asserted the store minted an
agent id, and the extension tool-filter tests were passing no `agentId` and relying on the
minted GUID resolving against a static config resolver.
