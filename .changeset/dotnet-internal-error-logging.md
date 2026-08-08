---
"@smooai/smooth-operator": patch
---

fix(dotnet): log the exception behind every `INTERNAL_ERROR`, and read the shared `SMOOAI_*` gateway env vars

Two bugs, one of which hid the other.

**th-e7ef23 — the swallowed exception.** Both places `FrameDispatcher` turns an exception into the
protocol's `INTERNAL_ERROR` (the dispatcher's outer guard and the spawned turn's guard) discarded the
exception entirely. A server whose every turn failed logged *nothing* — no exception, no stack, at any
level. Both sites now route through `LogInternalError`, which records the action, the requestId and the
exception (with stack) at `Error` level, falling back to stderr when no `ILogger` is wired. The wire
message is unchanged: still generic, still leaks no detail to the client.

**th-df7007 — every .NET turn returned `INTERNAL_ERROR`.** The host read the gateway URL/key/model from
`SMOOTH_GATEWAY_URL` / `SMOOTH_GATEWAY_KEY` / `SMOOTH_MODEL`, but rust, go, ts and python all read the
`SMOOAI_*` spelling. Any launcher or bench using the shared contract left the .NET host keyless, so every
turn hit the gateway with the literal key `"unset"` and got back `HTTP 401 … LiteLLM Virtual Key expected`
— surfaced as a bare `INTERNAL_ERROR` with zero tool calls. The host now reads `SMOOAI_GATEWAY_URL` /
`SMOOAI_GATEWAY_KEY` / `SMOOAI_MODEL` first, with the `SMOOTH_*` names kept as a fallback for existing
deployments.
