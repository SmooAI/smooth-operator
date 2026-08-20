---
'@smooai/smooth-operator': patch
---

go/typescript/python/dotnet-server: emit `interaction_required` **before** the raise tool's `toolCall` `stream_chunk`, matching the Rust reference.

For a Rich Interaction, the Rust server emits the park event first; Go, TypeScript, Python and .NET all emitted the raw `toolCall` chunk first. A client that renders tool calls therefore showed "calling `request_identity_intake`…" before the card it was calling for ever appeared — framework internals leaking ahead of the semantic event.

Ruled a port bug rather than a protocol variant, on the ports' own evidence: all five already defer the gated tool's chunk until after the prompt for the **other** park type (`hitl-write-confirmation`, which all five have always passed). The four were internally inconsistent between their two park paths while Rust was consistent across both.

The fix reuses each port's existing write-confirmation mechanism rather than inventing a second one: suppress the chunk in the engine stream loop on a tool-name predicate, then re-emit it from the park path immediately after the park event. Go already deferred interaction raises but re-emitted at the top of the raise tool, ahead of the park — that emit moved. TypeScript, Python and .NET gained the predicate (`isInteractionRaise` / `_is_interaction_raise` / `IsInteractionRaise`) matched against the hosted kinds' `request_<kind>` names — deliberately **not** the generic `submit_interaction` tool, whose chunk has no park event to follow and would simply be dropped. Every non-park exit of the raise tool (parse error, conversational fallback) emits the chunk immediately, so `interaction-conversational-fallback` is unaffected.

Verified against the shared conformance corpus, which pinned this order and marked the four as known divergences: `interaction-park-resume`, `interaction-declined`, `interaction-invalid-retryable` and `interaction-stale-id-rejected` now pass on all five servers, and their `knownDivergences` markers are removed. Rust is unchanged.

`interaction-choices-park-resume` keeps a `["go"]` marker, re-pointed at a different bug that was only reachable once the ordering was fixed: `splitIntoChunks` in `smooth-operator-core` Go slices the mock reply by bytes, so the em-dash in that scenario's reply is split mid-rune and arrives as U+FFFD. It needs a one-line upstream fix (slice `[]rune`) and a core release.
