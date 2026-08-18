---
'@smooai/smooth-operator': patch
---

Stop exporting fabricated token counts on the turn span, and adopt the cross-language `cost_unavailable` attribute.

`gen_ai.usage.input_tokens` was published whenever *either* count was non-zero. That `||` was the whole bug: when the gateway drops the usage chunk, `smooai-smooth-operator-core`'s `collect_stream` fabricates the struct — `prompt_tokens` hardcoded to `0` and `completion_tokens` estimated as `content.len() / 4` — so the fabricated struct always had a non-zero completion count, and the `||` published `input_tokens = 0` beside it. LiteLLM drops that chunk for `smooth-*` aliases, so this was the common path, not an edge case: no streamed turn has ever exported a measured token count.

The estimated output count looked plausible precisely *because* it is derived from the reply text — an estimate computed from the output cannot help but track the output. Only the zeroed input half looked obviously wrong.

Now gated on `prompt_tokens > 0` alone: both counts are exported, or neither. Absent is honest; `0` is a lie. The underlying fabrication still needs fixing in core.

Also adds `smooai.gen_ai.cost_unavailable = "unpriced"`, set instead of `gen_ai.usage.cost_usd` when no cost could be established — the same attribute name and value the TypeScript emitters use, so a consumer never has to special-case per engine.
