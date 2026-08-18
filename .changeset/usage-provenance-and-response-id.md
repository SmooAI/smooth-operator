---
'@smooai/smooth-operator': patch
---

Emit usage provenance, cost provenance, and the gateway response id on the turn span.

Closes a hole in the previous release. That change gated `gen_ai.usage.input_tokens` on `prompt_tokens > 0`, which worked only against the *streaming* estimator — the one that hardcodes prompt tokens to zero. Core has a **second** estimator on the non-streaming path that derives prompt tokens from the request JSON length, so it produces a plausible non-zero count. The old gate published that invented number as a measurement; a test restoring it prints the invented `372`.

Now reads `AgentEvent::Completed.usage_estimated` (core 1.10) instead of inferring provenance from the value. A flag carries the fact; a heuristic guesses at it, and this is the second bug today caused by inferring provenance from a plausible-looking number.

Adds:

- **`gen_ai.usage.cost_source`** — `gateway` or `estimated`, set alongside `cost_usd`. Without it a billable figure is indistinguishable from a guess against a local price table, which matters on a metered SKU.
- **`gen_ai.response.id`** — the gateway's `chatcmpl-…`, previously discarded at deserialization on all four LLM paths. It joins `LiteLLM_SpendLogs.request_id`, whose row carries the gateway's authoritative dollars *and* real token counts — so it matters most exactly when the counts on the span are absent.

Requires core 1.10; the wire protocol is unchanged (the flags are telemetry-only, pinned by a key-count assertion).
